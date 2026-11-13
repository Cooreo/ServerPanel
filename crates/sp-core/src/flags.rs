//! JVM flag presets and validation.
//!
//! The presets here are the documented community standards:
//! * **Aikar's flags** — <https://mcflags.emc.gs>, G1GC tuned for Minecraft's
//!   allocation pattern. The `-Dusing.aikars.flags` / `-Daikars.new.flags` pair is
//!   kept so admins can trace the origin, exactly as the upstream docs show.
//! * **Modern GC** — G1 with a lower pause target for 1.20+ on Java 17/21.
//! * **ZGC (low pause)** — for big heaps on Java 21+; note `-XX:+ZGenerational`
//!   was removed in JDK 23, so we only emit it for 21..=22.
//!
//! Flags here are *args*, not a shell string: they are passed to `Command` as an
//! argv vector, so quoting is impossible to get wrong (see `sp_process::spawn`).

use crate::error::CoreError;

/// Flags must never set the heap directly — the panel owns `-Xms`/`-Xmx` so the
/// per-server memory limit stays authoritative (and matches what the UI charts).
const HEAP_FLAGS: [&str; 6] = [
    "-Xms",
    "-Xmx",
    "-XX:InitialHeapSize",
    "-XX:MaxHeapSize",
    "-XX:MinHeapSize",
    "-XX:SoftMaxHeapSize",
];

pub const PRESET_NONE: &str = "none";
pub const PRESET_AIKAR: &str = "aikar";
pub const PRESET_MODERN_G1: &str = "modern-g1";
pub const PRESET_ZGC: &str = "zgc";

/// Aikar's Flags (G1GC), as published at https://mcflags.emc.gs.
pub fn aikar_flags() -> Vec<String> {
    [
        "-XX:+UseG1GC",
        "-XX:+ParallelRefProcEnabled",
        "-XX:MaxGCPauseMillis=200",
        "-XX:+UnlockExperimentalVMOptions",
        "-XX:+DisableExplicitGC",
        "-XX:+AlwaysPreTouch",
        "-XX:G1NewSizePercent=40",
        "-XX:G1MaxNewSizePercent=50",
        "-XX:G1HeapRegionSize=16M",
        "-XX:G1ReservePercent=15",
        "-XX:G1HeapWastePercent=5",
        "-XX:G1MixedGCCountTarget=4",
        "-XX:InitiatingHeapOccupancyPercent=20",
        "-XX:G1MixedGCLiveThresholdPercent=90",
        "-XX:G1RSetUpdatingPauseTimePercent=5",
        "-XX:SurvivorRatio=32",
        "-XX:+PerfDisableSharedMem",
        "-XX:MaxTenuringThreshold=1",
        // Kept for provenance, as the upstream instructions show them.
        "-Dusing.aikars.flags=https://mcflags.emc.gs",
        "-Daikars.new.flags=true",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// G1 with a tighter pause target and no provenance defines — a good default for
/// 1.21-era servers with 4-12 GB heaps.
pub fn modern_g1_flags() -> Vec<String> {
    [
        "-XX:+UseG1GC",
        "-XX:+ParallelRefProcEnabled",
        "-XX:MaxGCPauseMillis=150",
        "-XX:+PerfDisableSharedMem",
        "-XX:+DisableExplicitGC",
        "-XX:+AlwaysPreTouch",
        "-XX:InitiatingHeapOccupancyPercent=35",
        "-XX:G1HeapRegionSize=8M",
        "-XX:SurvivorRatio=8",
        "-XX:MaxTenuringThreshold=6",
        "-XX:+UseStringDeduplication",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// Generational ZGC. Emits `-XX:+ZGenerational` only for the JDKs that have it.
pub fn zgc_flags(java_major: u16) -> Vec<String> {
    let mut v = vec![
        "-XX:+UseZGC".to_string(),
        "-XX:+DisableExplicitGC".to_string(),
        "-XX:+AlwaysPreTouch".to_string(),
        "-XX:-OmitStackTraceInFastThrow".to_string(),
    ];
    if (21..=22).contains(&java_major) {
        v.push("-XX:+ZGenerational".to_string());
    }
    if java_major >= 24 {
        // `-XX:+UseNUMA` was reworked; keep the preset conservative and let admins
        // add JDK-specific tuning by hand in the config editor.
        v.push("-XX:-ZProactive".to_string());
    }
    v
}

/// Everything the UI can offer, so the wizard's dropdown and the backend accept
/// list stay in sync from one source.
pub fn presets() -> Vec<FlagPreset> {
    vec![
        FlagPreset {
            id: PRESET_AIKAR.into(),
            label: "Aikar's Flags (recommended)".into(),
            description: "G1GC tuned by Aikar for Minecraft's allocation pattern. Best all-round choice for Paper/Purpur/Sponge.".into(),
            flags: aikar_flags(),
            min_java_major: 8,
        },
        FlagPreset {
            id: PRESET_MODERN_G1.into(),
            label: "Modern G1".into(),
            description: "G1 with a 150 ms pause target and string dedup — good for 1.21+ with mid-size heaps.".into(),
            flags: modern_g1_flags(),
            min_java_major: 17,
        },
        FlagPreset {
            id: PRESET_ZGC.into(),
            label: "ZGC (low pause)".into(),
            description: "Generational ZGC. Very low pauses on large heaps; slightly higher CPU. Needs Java 21+.".into(),
            flags: zgc_flags(21),
            min_java_major: 21,
        },
        FlagPreset {
            id: PRESET_NONE.into(),
            label: "No preset (custom)".into(),
            description: "Only the flags you type yourself, plus the panel's heap and property flags.".into(),
            flags: Vec::new(),
            min_java_major: 8,
        },
    ]
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct FlagPreset {
    pub id: String,
    pub label: String,
    pub description: String,
    pub flags: Vec<String>,
    pub min_java_major: u16,
}

pub fn preset_flags(id: &str, java_major: u16) -> Vec<String> {
    match id {
        PRESET_AIKAR => aikar_flags(),
        PRESET_MODERN_G1 => modern_g1_flags(),
        PRESET_ZGC => zgc_flags(java_major),
        _ => Vec::new(),
    }
}

/// `-Xms`/`-Xmx` from the configured MiB. When `tune_heap` is false we only set
/// `-Xmx`, so admins who prefer a growing heap can opt out.
pub fn heap_flags(memory_mb: u32, tune_heap: bool) -> Vec<String> {
    let max = format!("-Xmx{memory_mb}M");
    if tune_heap {
        vec![format!("-Xms{memory_mb}M"), max]
    } else {
        vec![max]
    }
}

/// Reject flags the panel must own, or that would let a "flag" smuggle an
/// argument into the command line.
///
/// Rules: no heap flags, no `-agentlib`/`-javaagent` (arbitrary code loading is a
/// security boundary we keep at the panel level), no embedded NUL/newline, no
/// whitespace inside one arg (argv is built verbatim, so `"-Xmx 1G"` would be a
/// single bogus arg rather than two flags — better to reject than to confuse).
pub fn validate_flags(flags: &[String]) -> Result<(), CoreError> {
    for (i, raw) in flags.iter().enumerate() {
        let f = raw.trim();
        if f.is_empty() {
            return Err(CoreError::validation(
                "jvm_flags",
                format!("flag #{i} is empty"),
            ));
        }
        if f.chars().any(|c| c == '\0' || c == '\n' || c == '\r') {
            return Err(CoreError::validation(
                "jvm_flags",
                "flag contains a control character",
            ));
        }
        if f.contains(char::is_whitespace) {
            return Err(CoreError::validation(
                "jvm_flags",
                format!("`{f}` contains whitespace; pass it as two separate flags"),
            ));
        }
        for heap in HEAP_FLAGS {
            if f.starts_with(heap) {
                return Err(CoreError::validation(
                    "jvm_flags",
                    format!("{heap} is managed by the memory limit in this panel"),
                ));
            }
        }
        if f.starts_with("-agentlib") || f.starts_with("-agentpath") || f.starts_with("-javaagent")
        {
            return Err(CoreError::validation(
                "jvm_flags",
                "agent flags are not permitted; install the mod/plugin instead",
            ));
        }
        if !f.starts_with('-') {
            return Err(CoreError::validation(
                "jvm_flags",
                format!("`{f}` must start with `-` (it is a JVM flag, not a server argument)"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_listed_and_unique() {
        let p = presets();
        assert_eq!(p.len(), 4);
        let ids: Vec<&str> = p.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![PRESET_AIKAR, PRESET_MODERN_G1, PRESET_ZGC, PRESET_NONE]
        );
        assert!(p
            .iter()
            .all(|x| !x.label.is_empty() && !x.description.is_empty()));
    }

    #[test]
    fn aikar_flags_have_the_documented_shape() {
        let f = aikar_flags();
        assert!(f.contains(&"-XX:+UseG1GC".to_string()));
        assert!(f.iter().any(|x| x.starts_with("-Dusing.aikars.flags=")));
        assert!(f.iter().any(|x| x == "-Daikars.new.flags=true"));
        assert!(
            !f.iter().any(|x| x.starts_with("-Xm")),
            "heap is owned by the panel"
        );
        validate_flags(&f).expect("the shipped preset must validate");
        validate_flags(&modern_g1_flags()).expect("modern preset validates");
        validate_flags(&zgc_flags(21)).expect("zgc preset validates");
    }

    #[test]
    fn zgc_generational_only_where_it_exists() {
        assert!(zgc_flags(17).iter().any(|f| f == "-XX:+UseZGC"));
        assert!(!zgc_flags(17).iter().any(|f| f.contains("ZGenerational")));
        assert!(zgc_flags(21).iter().any(|f| f == "-XX:+ZGenerational"));
        assert!(zgc_flags(22).iter().any(|f| f == "-XX:+ZGenerational"));
        assert!(!zgc_flags(23).iter().any(|f| f.contains("ZGenerational")));
        assert!(zgc_flags(24).iter().any(|f| f == "-XX:-ZProactive"));
    }

    #[test]
    fn heap_flags_follow_toggle() {
        assert_eq!(heap_flags(2048, true), vec!["-Xms2048M", "-Xmx2048M"]);
        assert_eq!(heap_flags(4096, false), vec!["-Xmx4096M"]);
    }

    #[test]
    fn rejects_owned_and_dangerous_flags() {
        for bad in [
            vec!["-Xmx2G".into()],
            vec!["-XX:MaxHeapSize=2g".into()],
            vec!["-javaagent:/tmp/evil.jar".into()],
            vec!["-agentlib:jdwp=transport=dt_socket".into()],
            vec!["-Xmx 2G".into()],
            vec!["nogui".into()],
            vec!["  ".into()],
            vec!["-DDangerous\nnewline".into()],
        ] {
            assert!(validate_flags(&bad).is_err(), "should reject {bad:?}");
        }
        assert!(validate_flags(&["-XX:+UseNUMA".into(), "-Dfoo=bar".into()]).is_ok());
    }

    #[test]
    fn preset_lookup_by_id() {
        assert_eq!(preset_flags(PRESET_AIKAR, 21).len(), aikar_flags().len());
        assert!(preset_flags(PRESET_NONE, 21).is_empty());
        assert!(preset_flags("bogus", 21).is_empty());
        assert_eq!(preset_flags(PRESET_ZGC, 21).len(), zgc_flags(21).len());
    }
}
