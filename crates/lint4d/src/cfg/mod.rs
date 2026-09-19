pub mod analysis;
pub mod project_snapshot;

pub use cfg_pascal::PreparationEnvironment;
pub use project_snapshot::{
    CfgProjectSnapshot, CfgSnapshotError, CfgSnapshotOptions, CfgSnapshotStatus,
    to_cfg_project_snapshot,
};
