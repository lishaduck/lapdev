//! `SeaORM` Entity. Generated by sea-orm-codegen 0.12.4

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "prebuild")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub created_at: DateTimeWithTimeZone,
    pub deleted_at: Option<DateTimeWithTimeZone>,
    pub project_id: Uuid,
    pub user_id: Option<Uuid>,
    pub cores: String,
    pub branch: String,
    pub commit: String,
    pub host_id: Uuid,
    pub osuser: String,
    pub status: String,
    pub by_workspace: bool,
    pub build_output: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::prebuild_replica::Entity")]
    PrebuildReplica,
    #[sea_orm(
        belongs_to = "super::project::Entity",
        from = "Column::ProjectId",
        to = "super::project::Column::Id",
        on_update = "NoAction",
        on_delete = "NoAction"
    )]
    Project,
    #[sea_orm(
        belongs_to = "super::workspace_host::Entity",
        from = "Column::HostId",
        to = "super::workspace_host::Column::Id",
        on_update = "NoAction",
        on_delete = "NoAction"
    )]
    WorkspaceHost,
}

impl Related<super::prebuild_replica::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::PrebuildReplica.def()
    }
}

impl Related<super::project::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Project.def()
    }
}

impl Related<super::workspace_host::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::WorkspaceHost.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}