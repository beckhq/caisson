use crate::error::{CaissonError, Result};
use std::time::Duration;

pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * KIB;
pub const GIB: u64 = 1024 * MIB;
pub const TIB: u64 = 1024 * GIB;

/// Parse human-readable byte sizes such as `250G`, `50g`, `1.5T`, `512MiB`, `1024`.
/// Uses binary (1024-based) units matching macOS APFS `diskutil` conventions.
pub fn parse_size(input: &str) -> Result<u64> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(CaissonError::InvalidSize(input.to_string()));
    }

    let split_idx = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());

    let (num_part, suffix_part) = trimmed.split_at(split_idx);
    if num_part.is_empty() {
        return Err(CaissonError::InvalidSize(input.to_string()));
    }

    let value: f64 = num_part
        .parse()
        .map_err(|_| CaissonError::InvalidSize(input.to_string()))?;
    if !value.is_finite() || value < 0.0 {
        return Err(CaissonError::InvalidSize(input.to_string()));
    }

    let mult: u64 = match suffix_part.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => KIB,
        "m" | "mb" | "mib" => MIB,
        "g" | "gb" | "gib" => GIB,
        "t" | "tb" | "tib" => TIB,
        _ => return Err(CaissonError::InvalidSize(input.to_string())),
    };

    let bytes = value * (mult as f64);
    if !bytes.is_finite() || bytes > (u64::MAX as f64) {
        return Err(CaissonError::InvalidSize(input.to_string()));
    }
    Ok(bytes.round() as u64)
}

/// Format bytes as a compact binary human string (e.g., `250.0 GiB`, `512.0 MiB`).
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= TIB {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Format bytes for `diskutil apfs addVolume -quota <size>` (exact integer bytes with `b` suffix,
/// or clean `g`/`m` suffix when exact).
pub fn format_diskutil_size(bytes: u64) -> String {
    if bytes > 0 && bytes.is_multiple_of(GIB) {
        format!("{}g", bytes / GIB)
    } else if bytes > 0 && bytes.is_multiple_of(MIB) {
        format!("{}m", bytes / MIB)
    } else {
        format!("{bytes}b")
    }
}

/// Parse durations like `7d`, `24h`, `30m`, `90s`, `2w`.
pub fn parse_duration(input: &str) -> Result<Duration> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(CaissonError::InvalidDuration(input.to_string()));
    }

    let split_idx = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (num_part, suffix_part) = trimmed.split_at(split_idx);
    if num_part.is_empty() || suffix_part.is_empty() {
        return Err(CaissonError::InvalidDuration(input.to_string()));
    }

    let n: u64 = num_part
        .parse()
        .map_err(|_| CaissonError::InvalidDuration(input.to_string()))?;

    let secs = match suffix_part.trim().to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" => n,
        "m" | "min" | "mins" => n.saturating_mul(60),
        "h" | "hr" | "hrs" => n.saturating_mul(3600),
        "d" | "day" | "days" => n.saturating_mul(86_400),
        "w" | "wk" | "wks" => n.saturating_mul(86_400 * 7),
        _ => return Err(CaissonError::InvalidDuration(input.to_string())),
    };

    Ok(Duration::from_secs(secs))
}

/// Format a duration as a compact age string like `3d`, `5h`, `12m`, `45s`.
pub fn format_age(dur: Duration) -> String {
    let secs = dur.as_secs();
    if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binary_sizes() {
        assert_eq!(parse_size("250G").unwrap(), 250 * GIB);
        assert_eq!(parse_size("50g").unwrap(), 50 * GIB);
        assert_eq!(parse_size("512MiB").unwrap(), 512 * MIB);
        assert_eq!(parse_size("1.5T").unwrap(), (1.5 * TIB as f64) as u64);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("0G").unwrap(), 0);
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("10X").is_err());
    }

    #[test]
    fn formats_diskutil_sizes() {
        assert_eq!(format_diskutil_size(300 * GIB), "300g");
        assert_eq!(format_diskutil_size(512 * MIB), "512m");
        assert_eq!(format_diskutil_size(12345), "12345b");
    }

    #[test]
    fn parses_durations() {
        assert_eq!(
            parse_duration("7d").unwrap(),
            Duration::from_secs(7 * 86_400)
        );
        assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(86_400));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(
            parse_duration("2w").unwrap(),
            Duration::from_secs(14 * 86_400)
        );
        assert!(parse_duration("7").is_err());
        assert!(parse_duration("d").is_err());
        assert!(parse_duration("7y").is_err());
    }

    #[test]
    fn formats_ages() {
        assert_eq!(format_age(Duration::from_secs(10)), "10s");
        assert_eq!(format_age(Duration::from_secs(125)), "2m");
        assert_eq!(format_age(Duration::from_secs(7200)), "2h");
        assert_eq!(format_age(Duration::from_secs(90_000)), "1d");
    }
}
