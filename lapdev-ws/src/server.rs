use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use chrono::Utc;
use clap::Parser;
use docker_compose_types::{AdvancedBuildStep, BuildStep, Compose};
use futures::StreamExt;
use http_body_util::{BodyExt, Full};
use hyperlocal::{UnixClientExt, UnixConnector, Uri};
use lapdev_common::{
    devcontainer::{
        DevContainerCmd, DevContainerConfig, DevContainerCwd, DevContainerLifeCycleCmd,
    },
    BuildTarget, ContainerImageInfo, RepoBuildInfo, RepoBuildOutput, RepoComposeService,
};
use lapdev_rpc::{
    error::ApiError, spawn_twoway, ConductorServiceClient, InterWorkspaceService, WorkspaceService,
};
use netstat2::TcpState;
use serde::Deserialize;
use tarpc::{
    context::current,
    server::{BaseChannel, Channel},
    tokio_serde::formats::Bincode,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{Mutex, RwLock},
};
use uuid::Uuid;

use crate::service::{InterWorkspaceRpcService, WorkspaceRpcService};

pub const LAPDEV_WS_VERSION: &str = env!("CARGO_PKG_VERSION");
const INSTALL_SCRIPT: &[u8] = include_bytes!("../scripts/install_guest_agent.sh");
const LAPDEV_GUEST_AGENT: &[u8] = include_bytes!("../../target/release/lapdev-guest-agent");

#[derive(Clone, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct LapdevWsConfig {
    bind: Option<String>,
    ws_port: Option<u16>,
    inter_ws_port: Option<u16>,
}

#[derive(Parser)]
#[clap(name = "lapdev-ws")]
#[clap(version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    /// The config file path
    #[clap(short, long, action, value_hint = clap::ValueHint::AnyPath)]
    config_file: Option<PathBuf>,
}

#[derive(Clone)]
pub struct WorkspaceServer {
    pub rpcs: Arc<RwLock<Vec<WorkspaceRpcService>>>,
}

impl Default for WorkspaceServer {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let config_file = cli
        .config_file
        .unwrap_or_else(|| PathBuf::from("/etc/lapdev-ws.conf"));
    let config_content = tokio::fs::read_to_string(&config_file)
        .await
        .with_context(|| format!("can't read config file {}", config_file.to_string_lossy()))?;
    let config: LapdevWsConfig =
        toml::from_str(&config_content).with_context(|| "wrong config file format")?;
    let bind = config.bind.as_deref().unwrap_or("0.0.0.0");
    let ws_port = config.ws_port.unwrap_or(6123);
    let inter_ws_port = config.inter_ws_port.unwrap_or(6122);
    WorkspaceServer::new()
        .run(bind, ws_port, inter_ws_port)
        .await
}

impl WorkspaceServer {
    fn new() -> Self {
        Self {
            rpcs: Default::default(),
        }
    }

    async fn run(&self, bind: &str, ws_port: u16, inter_ws_port: u16) -> Result<()> {
        {
            let server = self.clone();
            let bind = bind.to_string();
            tokio::spawn(async move {
                if let Err(e) = server.run_inter_ws_service(&bind, inter_ws_port).await {
                    tracing::error!("run iter ws service error: {e:#}");
                }
            });
        }

        let mut listener =
            tarpc::serde_transport::tcp::listen((bind, ws_port), Bincode::default).await?;

        {
            let server = self.clone();
            tokio::spawn(async move {
                server.run_tasks().await;
            });
        }

        while let Some(conn) = listener.next().await {
            if let Ok(conn) = conn {
                let peer_addr = conn.peer_addr();
                let (server_chan, client_chan, _) = spawn_twoway(conn);
                let conductor_client =
                    ConductorServiceClient::new(tarpc::client::Config::default(), client_chan)
                        .spawn();
                let server = self.clone();

                let id = Uuid::new_v4();
                let rpc = WorkspaceRpcService {
                    id,
                    server,
                    conductor_client,
                };
                self.rpcs.write().await.push(rpc.clone());

                let rpcs = self.rpcs.clone();
                tokio::spawn(async move {
                    BaseChannel::with_defaults(server_chan)
                        .execute(rpc.serve())
                        .for_each(|resp| async move {
                            tokio::spawn(resp);
                        })
                        .await;
                    tracing::info!("incoming conductor connection {peer_addr:?} stopped");
                    rpcs.write().await.retain(|rpc| rpc.id != id);
                });
            }
        }

        Ok(())
    }

    async fn run_tasks(&self) {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            if let Err(e) = self.run_task().await {
                let err = if let ApiError::InternalError(e) = e {
                    e.to_string()
                } else {
                    e.to_string()
                };
                tracing::error!("run task error: {err}");
            }
        }
    }

    async fn run_task(&self) -> Result<(), ApiError> {
        let rpc = { self.rpcs.read().await.first().cloned() };
        let rpc = rpc.ok_or_else(|| anyhow!("don't have any conductor connections"))?;
        let workspaces = rpc.conductor_client.running_workspaces(current()).await??;

        let mut active_ports = HashMap::new();
        for si in netstat2::iterate_sockets_info_without_pids(
            netstat2::AddressFamilyFlags::IPV4 | netstat2::AddressFamilyFlags::IPV6,
            netstat2::ProtocolFlags::TCP,
        )?
        .flatten()
        {
            if let netstat2::ProtocolSocketInfo::Tcp(si) = si.protocol_socket_info {
                if si.state == TcpState::Established {
                    active_ports.insert(si.local_port as i32, si.local_port as i32);
                }
            }
        }

        for workspace in &workspaces {
            let mut active = false;
            if let Some(port) = workspace.ssh_port {
                active |= active_ports.contains_key(&port);
            }
            if let Some(port) = workspace.ide_port {
                active |= active_ports.contains_key(&port);
            }

            if active {
                // we have activity on the workspace, so we set last_inactivity to none
                // if it's not
                if workspace.last_inactivity.is_some() {
                    let _ = rpc
                        .conductor_client
                        .update_workspace_last_inactivity(current(), workspace.id, None)
                        .await;
                }
            } else {
                // we don't have activity, so if last_inactivity is none,
                // we make the current time as the last_inactivity
                if workspace.last_inactivity.is_none() {
                    let _ = rpc
                        .conductor_client
                        .update_workspace_last_inactivity(
                            current(),
                            workspace.id,
                            Some(Utc::now().into()),
                        )
                        .await;
                }
            }
        }

        Ok(())
    }

    async fn run_inter_ws_service(&self, bind: &str, inter_ws_port: u16) -> Result<()> {
        let mut listener =
            tarpc::serde_transport::tcp::listen((bind, inter_ws_port), Bincode::default).await?;
        listener.config_mut().max_frame_length(usize::MAX);
        listener
            // Ignore accept errors.
            .filter_map(|r| futures::future::ready(r.ok()))
            .map(tarpc::server::BaseChannel::with_defaults)
            // serve is generated by the service attribute. It takes as input any type implementing
            // the generated World trait.
            .map(|channel| {
                let server = InterWorkspaceRpcService {
                    server: self.clone(),
                };
                channel.execute(server.serve()).for_each(spawn)
            })
            // Max 10 channels.
            .buffer_unordered(100)
            .for_each(|_| async {})
            .await;
        Ok(())
    }

    fn podman_socket(&self, uid: &str) -> String {
        format!("/run/user/{uid}/podman/podman.sock")
    }

    async fn check_podman_socket(&self, osuser: &str, uid: &str) {
        tracing::debug!("check podman socket");
        if !tokio::fs::try_exists(self.podman_socket(uid))
            .await
            .unwrap_or(false)
        {
            tracing::debug!("podman socket doens't exist, start system service");
            let osuser = osuser.to_string();
            tokio::spawn(async move {
                let _ = Command::new("su")
                    .arg("-")
                    .arg(osuser)
                    .arg("-c")
                    .arg("podman system service --time=0")
                    .status()
                    .await;
                println!("podman system service finished");
            });
        }
    }

    async fn _os_user_uid(&self, osuser: &str) -> Result<String> {
        let output = Command::new("id").arg("-u").arg(osuser).output().await;
        if let Ok(output) = output {
            if output.status.success() {
                let uid = String::from_utf8(output.stdout)?.trim().to_string();
                self.check_podman_socket(osuser, &uid).await;
                return Ok(uid);
            }
        }
        Err(anyhow!("no user"))
    }

    pub async fn os_user_uid(&self, username: &str) -> Result<String, ApiError> {
        if let Ok(uid) = self._os_user_uid(username).await {
            return Ok(uid);
        }

        if !Command::new("useradd")
            .arg(username)
            .arg("-d")
            .arg(format!("/home/{username}"))
            .arg("-m")
            .status()
            .await?
            .success()
        {
            return Err(anyhow!("can't do useradd {username}").into());
        }

        Command::new("su")
            .arg("-")
            .arg(username)
            .arg("-c")
            .arg(format!("mkdir /home/{username}/workspaces/"))
            .status()
            .await?;

        let uid = self._os_user_uid(username).await?;

        Command::new("loginctl")
            .arg("enable-linger")
            .arg(&uid)
            .status()
            .await?;

        Ok(uid)
    }

    pub async fn get_devcontainer(
        &self,
        info: &RepoBuildInfo,
    ) -> Result<Option<(DevContainerCwd, DevContainerConfig)>, ApiError> {
        let folder = PathBuf::from(self.build_repo_folder(info));
        let devcontainer_folder_path = folder.join(".devcontainer").join("devcontainer.json");
        let devcontainer_root_path = folder.join(".devcontainer.json");
        let (cwd, file_path) = if tokio::fs::try_exists(&devcontainer_folder_path)
            .await
            .unwrap_or(false)
        {
            (folder.join(".devcontainer"), devcontainer_folder_path)
        } else if tokio::fs::try_exists(&devcontainer_root_path)
            .await
            .unwrap_or(false)
        {
            (folder, devcontainer_root_path)
        } else {
            return Ok(None);
        };
        let content = tokio::fs::read_to_string(&file_path).await?;
        let config: DevContainerConfig = json5::from_str(&content)
            .map_err(|e| ApiError::RepositoryInvalid(format!("devcontainer.json invalid: {e}")))?;
        Ok(Some((cwd, config)))
    }

    async fn do_build_container_image(
        &self,
        conductor_client: &ConductorServiceClient,
        info: &RepoBuildInfo,
        cwd: &Path,
        context: &Path,
        dockerfile_content: &str,
        tag: &str,
    ) -> Result<(), ApiError> {
        let temp = tempfile::NamedTempFile::new()?.into_temp_path();
        {
            let mut temp_docker_file = tokio::fs::File::create(&temp).await?;
            temp_docker_file
                .write_all(dockerfile_content.as_bytes())
                .await?;
            temp_docker_file.write_all(b"\nUSER root\n").await?;
            temp_docker_file
                .write_all(b"COPY lapdev-guest-agent /lapdev-guest-agent\n")
                .await?;
            temp_docker_file
                .write_all(b"RUN chmod +x /lapdev-guest-agent\n")
                .await?;
            temp_docker_file
                .write_all(b"COPY install_guest_agent.sh /install_guest_agent.sh\n")
                .await?;
            temp_docker_file
                .write_all(b"RUN sh /install_guest_agent.sh\n")
                .await?;
            temp_docker_file
                .write_all(b"RUN rm /install_guest_agent.sh\n")
                .await?;
        }

        let install_script_path = context.join("install_guest_agent.sh");
        {
            let mut install_script_file = tokio::fs::File::create(&install_script_path).await?;
            install_script_file.write_all(INSTALL_SCRIPT).await?;
        }

        let lapdev_guest_agent_path = context.join("lapdev-guest-agent");
        {
            let mut file = tokio::fs::File::create(&lapdev_guest_agent_path).await?;
            file.write_all(LAPDEV_GUEST_AGENT).await?;
            file.flush().await?;
        }

        tokio::process::Command::new("chown")
            .arg(format!("{}:{}", info.osuser, info.osuser))
            .arg(&install_script_path)
            .output()
            .await?;
        tokio::process::Command::new("chown")
            .arg(format!("{}:{}", info.osuser, info.osuser))
            .arg(&temp)
            .output()
            .await?;

        let build_args = info
            .env
            .iter()
            .map(|(name, value)| format!("--build-arg {name}={value}"))
            .collect::<Vec<String>>()
            .join(" ");

        let mut child = tokio::process::Command::new("su")
            .arg("-")
            .arg(&info.osuser)
            .arg("-c")
            .arg(format!(
                "cd {} && podman build --no-cache {build_args} --cpuset-cpus {} -m {}g -f {} -t {tag} {}",
                cwd.to_string_lossy(),
                info.cpus
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<String>>()
                    .join(","),
                info.memory,
                temp.to_string_lossy(),
                context.to_string_lossy(),
            ))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stderr_log = self
            .update_build_std_output(conductor_client, &mut child, &info.target)
            .await;
        let status = child.wait().await?;
        if !status.success() {
            return Err(ApiError::RepositoryInvalid(format!(
                "Container Image build failed: {:?}",
                stderr_log.lock().await
            )));
        }

        let _ = tokio::fs::remove_file(&install_script_path).await;
        let _ = tokio::fs::remove_file(&lapdev_guest_agent_path).await;

        Ok(())
    }

    pub async fn build_container_image_from_base(
        &self,
        conductor_client: &ConductorServiceClient,
        info: &RepoBuildInfo,
        cwd: &Path,
        image: &str,
        tag: &str,
    ) -> Result<(), ApiError> {
        let _ = self
            .pull_container_image(conductor_client, &info.osuser, image, &info.target)
            .await;
        let image_info = self.container_image_info(&info.osuser, image).await?;

        let context = cwd.to_path_buf();
        let mut dockerfile_content = format!("FROM {image}\n");
        if let Some(entrypoint) = image_info.config.entrypoint {
            if !entrypoint.is_empty() {
                if let Ok(entrypoint) = serde_json::to_string(&entrypoint) {
                    dockerfile_content += "ENTRYPOINT ";
                    dockerfile_content += &entrypoint;
                    dockerfile_content += "\n";
                }
            }
        }
        if let Some(cmd) = image_info.config.cmd {
            if !cmd.is_empty() {
                if let Ok(cmd) = serde_json::to_string(&cmd) {
                    dockerfile_content += "CMD ";
                    dockerfile_content += &cmd;
                    dockerfile_content += "\n";
                }
            }
        }
        if let Some(ports) = image_info.config.exposed_ports {
            for port in ports.keys() {
                dockerfile_content += "EXPOSE ";
                dockerfile_content += port;
                dockerfile_content += "\n";
            }
        }

        self.do_build_container_image(
            conductor_client,
            info,
            cwd,
            &context,
            &dockerfile_content,
            tag,
        )
        .await?;
        Ok(())
    }

    pub async fn pull_container_image(
        &self,
        conductor_client: &ConductorServiceClient,
        osuser: &str,
        image: &str,
        target: &BuildTarget,
    ) -> Result<()> {
        let mut child = tokio::process::Command::new("su")
            .arg("-")
            .arg(osuser)
            .arg("-c")
            .arg(format!("podman pull {image}"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        self.update_build_std_output(conductor_client, &mut child, target)
            .await;
        let _ = child.wait().await;
        Ok(())
    }

    pub async fn container_image_info(
        &self,
        osuser: &str,
        image: &str,
    ) -> Result<ContainerImageInfo, ApiError> {
        let uid = self.os_user_uid(osuser).await?;
        let socket = &format!("/run/user/{uid}/podman/podman.sock");
        let url = Uri::new(socket, &format!("/images/{image}/json"));
        let client = unix_client();
        let req = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(url)
            .body(Full::<Bytes>::new(Bytes::new()))?;
        let resp = client.request(req).await?;
        let status = resp.status();
        let body = resp.collect().await?.to_bytes();
        if status != 200 {
            let err = String::from_utf8(body.to_vec())?;
            return Err(anyhow!(err).into());
        }
        let image_info: ContainerImageInfo = serde_json::from_slice(&body)?;
        Ok(image_info)
    }

    pub async fn build_container_image(
        &self,
        conductor_client: &ConductorServiceClient,
        info: &RepoBuildInfo,
        cwd: &Path,
        build: &AdvancedBuildStep,
        tag: &str,
    ) -> Result<(), ApiError> {
        let context = cwd.join(&build.context);
        let dockerfile = build.dockerfile.as_deref().unwrap_or("Dockerfile");
        let dockerfile = context.join(dockerfile);

        let dockerfile_content = tokio::fs::read_to_string(dockerfile)
            .await
            .map_err(|e| ApiError::RepositoryInvalid(format!("can't read dockerfile: {e}")))?;
        self.do_build_container_image(
            conductor_client,
            info,
            cwd,
            &context,
            &dockerfile_content,
            tag,
        )
        .await?;
        Ok(())
    }

    fn compose_service_env(
        &self,
        service: &docker_compose_types::Service,
    ) -> Vec<(String, String)> {
        match &service.environment {
            docker_compose_types::Environment::List(list) => list
                .iter()
                .filter_map(|s| {
                    let parts: Vec<String> = s.splitn(2, '=').map(|s| s.to_string()).collect();
                    if parts.len() == 2 {
                        Some((parts[0].clone(), parts[1].clone()))
                    } else {
                        None
                    }
                })
                .collect(),
            docker_compose_types::Environment::KvPair(pair) => pair
                .iter()
                .filter_map(|(key, value)| Some((key.to_string(), format!("{}", value.as_ref()?))))
                .collect(),
        }
    }

    async fn build_compose_service(
        &self,
        conductor_client: &ConductorServiceClient,
        info: &RepoBuildInfo,
        cwd: &Path,
        service: &docker_compose_types::Service,
        tag: &str,
    ) -> Result<(), ApiError> {
        if let Some(build) = &service.build_ {
            let build = match build {
                BuildStep::Simple(context) => AdvancedBuildStep {
                    context: context.to_string(),
                    ..Default::default()
                },
                BuildStep::Advanced(build) => build.to_owned(),
            };
            self.build_container_image(conductor_client, info, cwd, &build, tag)
                .await?;
        } else if let Some(image) = &service.image {
            self.build_container_image_from_base(conductor_client, info, cwd, image, tag)
                .await?;
        } else {
            return Err(ApiError::RepositoryInvalid(
                "can't find image or build in this compose service".to_string(),
            ));
        }
        Ok(())
    }

    pub async fn build_compose(
        &self,
        conductor_client: &ConductorServiceClient,
        info: &RepoBuildInfo,
        compose_file: &Path,
        tag: &str,
    ) -> Result<RepoBuildOutput, ApiError> {
        let content = tokio::fs::read_to_string(compose_file)
            .await
            .map_err(|e| ApiError::RepositoryInvalid(format!("can't read compose file: {e}")))?;
        let compose: Compose = serde_yaml::from_str(&content)
            .map_err(|e| ApiError::RepositoryInvalid(format!("can't parse compose file: {e}")))?;
        let cwd = compose_file
            .parent()
            .ok_or_else(|| anyhow!("compose file doens't have a parent directory"))?;
        let mut services = Vec::new();
        for (name, service) in compose.services.0 {
            if let Some(service) = service {
                let tag = format!("{tag}:{name}");
                self.build_compose_service(conductor_client, info, cwd, &service, &tag)
                    .await?;
                let env = self.compose_service_env(&service);
                services.push(RepoComposeService {
                    name,
                    image: tag,
                    env,
                });
            }
        }
        Ok(RepoBuildOutput::Compose(services))
    }

    pub fn repo_target_image_tag(&self, target: &BuildTarget) -> String {
        match target {
            BuildTarget::Workspace { name, .. } => name.clone(),
            BuildTarget::Prebuild(id) => id.to_string(),
        }
    }

    pub async fn run_lifecycle_commands(
        &self,
        conductor_client: &ConductorServiceClient,
        repo: &RepoBuildInfo,
        output: &RepoBuildOutput,
        config: &DevContainerConfig,
    ) {
        if let Some(cmd) = config.on_create_command.as_ref() {
            let _ = self
                .run_lifecycle_command(conductor_client, repo, output, config, cmd)
                .await;
        }
        if let Some(cmd) = config.update_content_command.as_ref() {
            let _ = self
                .run_lifecycle_command(conductor_client, repo, output, config, cmd)
                .await;
        }
        if let Some(cmd) = config.post_create_command.as_ref() {
            let _ = self
                .run_lifecycle_command(conductor_client, repo, output, config, cmd)
                .await;
        }
    }

    async fn run_lifecycle_command(
        &self,
        conductor_client: &ConductorServiceClient,
        repo: &RepoBuildInfo,
        output: &RepoBuildOutput,
        config: &DevContainerConfig,
        cmd: &DevContainerLifeCycleCmd,
    ) -> Result<()> {
        match output {
            RepoBuildOutput::Compose(services) => {
                let cmd = match cmd {
                    DevContainerLifeCycleCmd::Simple(cmd) => {
                        DevContainerCmd::Simple(cmd.to_string())
                    }
                    DevContainerLifeCycleCmd::Args(cmds) => DevContainerCmd::Args(cmds.to_owned()),
                    DevContainerLifeCycleCmd::Object(cmds) => {
                        for (service, cmd) in cmds {
                            if let Some(service) = services.iter().find(|s| &s.name == service) {
                                self.run_devcontainer_command(
                                    conductor_client,
                                    repo,
                                    &service.image,
                                    cmd,
                                )
                                .await?;
                            }
                        }
                        return Ok(());
                    }
                };
                // if it's a single command, then we run it on main compose service
                if let Some(service) = config.service.as_ref() {
                    if let Some(service) = services.iter().find(|s| &s.name == service) {
                        self.run_devcontainer_command(conductor_client, repo, &service.image, &cmd)
                            .await?;
                    }
                }
            }
            RepoBuildOutput::Image(tag) => {
                let cmd = match cmd {
                    DevContainerLifeCycleCmd::Simple(cmd) => {
                        DevContainerCmd::Simple(cmd.to_string())
                    }
                    DevContainerLifeCycleCmd::Args(cmds) => DevContainerCmd::Args(cmds.to_owned()),
                    DevContainerLifeCycleCmd::Object(_) => {
                        return Err(anyhow!("can't use object cmd for non compose"))
                    }
                };
                self.run_devcontainer_command(conductor_client, repo, tag, &cmd)
                    .await?;
            }
        }
        Ok(())
    }

    pub fn workspace_folder(&self, osuser: &str, workspace_name: &str) -> String {
        format!("/home/{osuser}/workspaces/{workspace_name}")
    }

    pub fn prebuild_folder(&self, osuser: &str, prebuild_id: Uuid) -> String {
        format!("/home/{osuser}/workspaces/{prebuild_id}")
    }

    pub fn build_repo_folder(&self, info: &RepoBuildInfo) -> String {
        let target = match &info.target {
            BuildTarget::Workspace { name, .. } => name.to_string(),
            BuildTarget::Prebuild(id) => id.to_string(),
        };
        format!(
            "/home/{}/workspaces/{target}/{}",
            info.osuser, info.repo_name
        )
    }

    async fn run_devcontainer_command(
        &self,
        conductor_client: &ConductorServiceClient,
        info: &RepoBuildInfo,
        image: &str,
        cmd: &DevContainerCmd,
    ) -> Result<()> {
        let repo_folder = self.build_repo_folder(info);

        let cmd = match cmd {
            DevContainerCmd::Simple(cmd) => cmd.to_string(),
            DevContainerCmd::Args(cmds) => cmds.join(" "),
        };
        let mut child = Command::new("su")
            .arg("-")
            .arg(&info.osuser)
            .arg("-c")
            .arg(format!(
                "podman run --rm --cpuset-cpus {} -m {}g --security-opt label=disable -v {repo_folder}:/workspace -w /workspace --user root --entrypoint \"\" {image} {cmd}",
                info.cpus
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<String>>()
                    .join(","),
                info.memory,
            ))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        self.update_build_std_output(conductor_client, &mut child, &info.target)
            .await;
        child.wait().await?;
        Ok(())
    }

    pub async fn update_build_std_output(
        &self,
        conductor_client: &ConductorServiceClient,
        child: &mut tokio::process::Child,
        target: &BuildTarget,
    ) -> Arc<Mutex<Vec<String>>> {
        if let Some(stdout) = child.stdout.take() {
            let conductor_client = conductor_client.clone();
            let target = target.clone();
            let mut reader = BufReader::new(stdout);
            tokio::spawn(async move {
                let mut line = String::new();
                while let Ok(n) = reader.read_line(&mut line).await {
                    if n > 0 {
                        let line = line.trim_end().to_string();
                        let _ = conductor_client
                            .update_build_repo_stdout(current(), target.clone(), line)
                            .await;
                    } else {
                        break;
                    }
                    line.clear();
                }
            });
        }

        let stderr_log = Arc::new(Mutex::new(Vec::new()));
        if let Some(stderr) = child.stderr.take() {
            let stderr_log = stderr_log.clone();
            let conductor_client = conductor_client.clone();
            let target = target.clone();
            let mut reader = BufReader::new(stderr);
            tokio::spawn(async move {
                let mut line = String::new();
                while let Ok(n) = reader.read_line(&mut line).await {
                    if n > 0 {
                        let line = line.trim_end().to_string();
                        let _ = conductor_client
                            .update_build_repo_stderr(current(), target.clone(), line.clone())
                            .await;
                        stderr_log.lock().await.push(line);
                    } else {
                        break;
                    }
                    line.clear();
                }
            });
        }
        stderr_log
    }

    pub async fn delete_image(&self, osuser: &str, image: &str) -> Result<()> {
        let uid = {
            let stdout = Command::new("id")
                .arg("-u")
                .arg(osuser)
                .output()
                .await?
                .stdout;
            String::from_utf8(stdout)?
        };
        let uid = uid.trim();
        let socket = format!("/run/user/{uid}/podman/podman.sock");

        let client = unix_client();
        {
            let url = Uri::new(&socket, &format!("/images/{image}"));
            let req = hyper::Request::builder()
                .method(hyper::Method::DELETE)
                .uri(url)
                .body(Full::<Bytes>::new(Bytes::new()))?;
            let resp = client.request(req).await?;
            let status = resp.status();
            if status != 200 && status != 404 {
                let body = resp.collect().await?.to_bytes();
                let err = String::from_utf8(body.to_vec())?;
                return Err(anyhow!("delete image error: {err}"));
            }
        }

        Ok(())
    }

    pub async fn delete_network(&self, osuser: &str, network: &str) -> Result<()> {
        let uid = {
            let stdout = Command::new("id")
                .arg("-u")
                .arg(osuser)
                .output()
                .await?
                .stdout;
            String::from_utf8(stdout)?
        };
        let uid = uid.trim();
        let socket = format!("/run/user/{uid}/podman/podman.sock");

        let client = unix_client();
        {
            let url = Uri::new(&socket, &format!("/networks/{network}"));
            let req = hyper::Request::builder()
                .method(hyper::Method::DELETE)
                .uri(url)
                .body(Full::<Bytes>::new(Bytes::new()))?;
            let resp = client.request(req).await?;
            let status = resp.status();
            if status != 204 && status != 404 {
                let body = resp.collect().await?.to_bytes();
                let err = String::from_utf8(body.to_vec())?;
                return Err(anyhow!("delete network error: {err}"));
            }
        }

        Ok(())
    }
}

async fn spawn(fut: impl futures::Future<Output = ()> + Send + 'static) {
    tokio::spawn(fut);
}

pub fn unix_client(
) -> hyper_util::client::legacy::Client<UnixConnector, http_body_util::Full<hyper::body::Bytes>> {
    hyper_util::client::legacy::Client::unix()
}
