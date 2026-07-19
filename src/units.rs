//! Human-readable quantities, one style everywhere: `512 B`, `8.0 KiB`, `1.3 GiB` — every display
//! (progress lines, conclusions, scan summaries) formats through here, so the program never shows
//! the same number two ways.

use std::time::Duration;

/// Bytes → `512 B` / `8.0 KiB` / `5.0 MiB` … (binary units, one decimal above bytes).
pub(crate) fn human_bytes(n: u64) -> String {
    const U: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let (mut v, mut i) = (n as f64, 0usize);
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

/// Signed byte delta: `+4.0 MiB` / `-8.3 GiB` (for net-change tables).
pub(crate) fn signed_bytes(n: i64) -> String {
    let sign = if n < 0 { "-" } else { "+" };
    format!("{sign}{}", human_bytes(n.unsigned_abs()))
}

/// Elapsed time → `45s` / `2m5s` / `1h1m`.
pub(crate) fn human_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_use_one_uniform_style() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(8 << 10), "8.0 KiB");
        assert_eq!(human_bytes(5 << 20), "5.0 MiB");
        assert_eq!(human_bytes(3 << 30), "3.0 GiB");
    }

    #[test]
    fn signed_bytes_carry_their_sign() {
        assert_eq!(signed_bytes(4 << 20), "+4.0 MiB");
        assert_eq!(signed_bytes(-(3 << 10)), "-3.0 KiB");
    }

    #[test]
    fn elapsed_is_humanized() {
        assert_eq!(human_elapsed(Duration::from_secs(45)), "45s");
        assert_eq!(human_elapsed(Duration::from_secs(125)), "2m5s");
        assert_eq!(human_elapsed(Duration::from_secs(3700)), "1h1m");
    }
}
