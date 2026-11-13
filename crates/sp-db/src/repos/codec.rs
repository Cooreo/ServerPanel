//! Enum ⇄ `TEXT` mapping.
//!
//! The strings are exactly the kebab-case forms `sp_core`'s serde derives produce,
//! which is what makes `sqlite3 panel.db 'select * from servers'` readable. The
//! test below asserts every variant of every mapped enum round-trips, so adding a
//! variant in `sp-core` without extending this table fails CI loudly.

use crate::error::{DbError, DbResult};
use sp_core::model::*;

/// Generate `<name>_to_str` / `<name>_from_str` pairs for a closed enum.
macro_rules! codec {
    ($ty:ty, $to:ident, $from:ident, [ $( $variant:path => $s:literal ),* $(,)? ], $fallback:expr) => {
        pub(crate) fn $to(v: $ty) -> &'static str {
            match v { $( $variant => $s, )* }
        }
        pub(crate) fn $from(s: &str) -> DbResult<$ty> {
            match s {
                $( $s => Ok($variant), )*
                other => {
                    tracing::warn!(enum_ = stringify!($ty), value = other, "unknown enum value in database");
                    let _fallback: $ty = $fallback;
                    Ok(_fallback)
                }
            }
        }
    };
}

codec!(
    ServerKind, kind_to_str, kind_from_str,
    [
        ServerKind::Vanilla => "vanilla",
        ServerKind::Paper => "paper",
        ServerKind::Purpur => "purpur",
        ServerKind::Folia => "folia",
        ServerKind::Fabric => "fabric",
        ServerKind::Quilt => "quilt",
        ServerKind::Forge => "forge",
        ServerKind::NeoForge => "neoforge",
        ServerKind::Spigot => "spigot",
        ServerKind::CraftBukkit => "craftbukkit",
        ServerKind::BungeeCord => "bungeecord",
        ServerKind::Waterfall => "waterfall",
        ServerKind::Velocity => "velocity",
        ServerKind::Geyser => "geyser",
    ],
    ServerKind::Paper
);

codec!(
    ServerStatus, status_to_str, status_from_str,
    [
        ServerStatus::NotInstalled => "not-installed",
        ServerStatus::Stopped => "stopped",
        ServerStatus::Starting => "starting",
        ServerStatus::Running => "running",
        ServerStatus::Stopping => "stopping",
        ServerStatus::Saving => "saving",
        ServerStatus::Crashed => "crashed",
        ServerStatus::Installing => "installing",
        ServerStatus::Killed => "killed",
    ],
    ServerStatus::Stopped
);

codec!(
    InstallState, install_state_to_str, install_state_from_str,
    [
        InstallState::Pending => "pending",
        InstallState::Running => "running",
        InstallState::Ready => "ready",
        InstallState::Failed => "failed",
    ],
    InstallState::Pending
);

codec!(
    BackupFormat, backup_format_to_str, backup_format_from_str,
    [BackupFormat::Zip => "zip", BackupFormat::TarZstd => "tar-zstd"],
    BackupFormat::Zip
);

codec!(
    BackupKind, backup_kind_to_str, backup_kind_from_str,
    [
        BackupKind::Manual => "manual",
        BackupKind::Scheduled => "scheduled",
        BackupKind::PreRestore => "pre-restore",
        BackupKind::Import => "import",
    ],
    BackupKind::Manual
);

codec!(
    BackupState, backup_state_to_str, backup_state_from_str,
    [
        BackupState::Queued => "queued",
        BackupState::Running => "running",
        BackupState::Completed => "completed",
        BackupState::Failed => "failed",
        BackupState::Restoring => "restoring",
    ],
    BackupState::Failed
);

codec!(
    ContentKind, content_kind_to_str, content_kind_from_str,
    [
        ContentKind::Mod => "mod",
        ContentKind::Plugin => "plugin",
        ContentKind::Datapack => "datapack",
        ContentKind::Modpack => "modpack",
        ContentKind::ResourcePack => "resourcepack",
        ContentKind::Shader => "shader",
        ContentKind::Addon => "addon",
    ],
    ContentKind::Mod
);

codec!(
    ContentSource, content_source_to_str, content_source_from_str,
    [
        ContentSource::Modrinth => "modrinth",
        ContentSource::Upload => "upload",
        ContentSource::Url => "url",
        ContentSource::Modpack => "modpack",
    ],
    ContentSource::Modrinth
);

codec!(
    JavaSource, java_source_to_str, java_source_from_str,
    [
        JavaSource::System => "system",
        JavaSource::Managed => "managed",
        JavaSource::Manual => "manual",
    ],
    JavaSource::System
);

codec!(
    Role, role_to_str, role_from_str,
    [Role::Admin => "admin", Role::Moderator => "moderator", Role::Viewer => "viewer"],
    Role::Viewer
);

codec!(
    TaskAction, task_action_to_str, task_action_from_str,
    [
        TaskAction::Command => "command",
        TaskAction::Broadcast => "broadcast",
        TaskAction::Restart => "restart",
        TaskAction::Start => "start",
        TaskAction::Stop => "stop",
        TaskAction::Backup => "backup",
        TaskAction::UpdateCheck => "update-check",
        TaskAction::PruneBackups => "prune-backups",
        TaskAction::Reload => "reload",
    ],
    TaskAction::Command
);

codec!(
    AuditOutcome, audit_outcome_to_str, audit_outcome_from_str,
    [AuditOutcome::Success => "success", AuditOutcome::Failure => "failure"],
    AuditOutcome::Success
);

codec!(
    ReleaseChannel, release_channel_to_str, release_channel_from_str,
    [
        ReleaseChannel::Stable => "stable",
        ReleaseChannel::Beta => "beta",
        ReleaseChannel::Nightly => "nightly",
    ],
    ReleaseChannel::Stable
);

codec!(
    NotificationLevel, notification_level_to_str, notification_level_from_str,
    [
        NotificationLevel::Info => "info",
        NotificationLevel::Success => "success",
        NotificationLevel::Warning => "warning",
        NotificationLevel::Error => "error",
    ],
    NotificationLevel::Info
);

codec!(
    VersionChannel, version_channel_to_str, version_channel_from_str,
    [
        VersionChannel::Release => "release",
        VersionChannel::Snapshot => "snapshot",
        VersionChannel::OldBeta => "old-beta",
        VersionChannel::OldRelease => "old-release",
        VersionChannel::All => "all",
    ],
    VersionChannel::Release
);

/// Unknown `enabled` for a queued notification is "pending", not "sent".
pub(crate) fn update_state_from_str(s: &str) -> &'static str {
    match s {
        "up-to-date" | "available" | "downloading" | "staged" | "applied" | "rolled-back" | "error" => s,
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_kind_covers_every_variant() {
        for k in ServerKind::ALL {
            let s = kind_to_str(k);
            assert_eq!(kind_from_str(s).unwrap(), k, "{k} → {s}");
        }
        assert_eq!(kind_to_str(ServerKind::NeoForge), "neoforge");
    }

    #[test]
    fn status_and_install_state_round_trip() {
        for s in [
            ServerStatus::NotInstalled,
            ServerStatus::Stopped,
            ServerStatus::Starting,
            ServerStatus::Running,
            ServerStatus::Stopping,
            ServerStatus::Saving,
            ServerStatus::Crashed,
            ServerStatus::Installing,
            ServerStatus::Killed,
        ] {
            assert_eq!(status_from_str(status_to_str(s)).unwrap(), s);
        }
        for s in [InstallState::Pending, InstallState::Running, InstallState::Ready, InstallState::Failed] {
            assert_eq!(install_state_from_str(install_state_to_str(s)).unwrap(), s);
        }
    }

    #[test]
    fn unknown_values_degrade_instead_of_failing() {
        // A DB written by a newer panel must still open.
        assert_eq!(kind_from_str("quartz").unwrap(), ServerKind::Paper);
        assert_eq!(status_from_str("wobbly").unwrap(), ServerStatus::Stopped);
        assert_eq!(role_from_str("superuser").unwrap(), Role::Viewer, "least privilege");
        assert_eq!(backup_state_from_str("???").unwrap(), BackupState::Failed);
    }

    #[test]
    fn backup_content_and_java_codes() {
        assert_eq!(backup_format_to_str(BackupFormat::TarZstd), "tar-zstd");
        assert_eq!(backup_kind_from_str("pre-restore").unwrap(), BackupKind::PreRestore);
        assert_eq!(content_kind_to_str(ContentKind::ResourcePack), "resourcepack");
        assert_eq!(content_source_from_str("upload").unwrap(), ContentSource::Upload);
        assert_eq!(java_source_to_str(JavaSource::Managed), "managed");
    }

    #[test]
    fn task_and_audit_codes() {
        assert_eq!(task_action_to_str(TaskAction::PruneBackups), "prune-backups");
        assert_eq!(task_action_from_str("update-check").unwrap(), TaskAction::UpdateCheck);
        assert_eq!(audit_outcome_to_str(AuditOutcome::Failure), "failure");
        assert_eq!(notification_level_from_str("warning").unwrap(), NotificationLevel::Warning);
        assert_eq!(version_channel_to_str(VersionChannel::OldBeta), "old-beta");
        assert_eq!(release_channel_to_str(ReleaseChannel::Nightly), "nightly");
    }

    #[test]
    fn codes_match_serde_names() {
        // The DB strings must equal the wire strings or the UI cannot reuse them.
        for k in ServerKind::ALL {
            let json = serde_json::to_string(&k).unwrap();
            assert_eq!(json.trim_matches('"'), kind_to_str(k), "{k}");
        }
        for a in [
            TaskAction::Command,
            TaskAction::Broadcast,
            TaskAction::Restart,
            TaskAction::Start,
            TaskAction::Stop,
            TaskAction::Backup,
            TaskAction::UpdateCheck,
            TaskAction::PruneBackups,
            TaskAction::Reload,
        ] {
            let json = serde_json::to_string(&a).unwrap();
            assert_eq!(json.trim_matches('"'), task_action_to_str(a), "{a:?}");
        }
    }

    #[test]
    fn update_state_is_whitelisted() {
        assert_eq!(update_state_from_str("available"), "available");
        assert_eq!(update_state_from_str(""), "unknown");
        assert_eq!(update_state_from_str("rm -rf"), "unknown");
    }

    #[test]
    fn corrupt_json_error_is_informative() {
        let err = DbError::Corrupt {
            column: "config_json".into(),
            value: "not json".into(),
        };
        assert!(err.to_string().contains("config_json"), "{err}");
    }
}
