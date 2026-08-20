//! Scene-name parsing: turn "Show.S02E05.1080p.WEB-DL.DDP5.1.H.264-GROUP"
//! into structured fields for badges, filtering and dedupe grouping.

use serde::Serialize;

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedRelease {
    pub resolution: Option<String>,   // 2160p / 1080p / 720p / 480p
    pub source: Option<String>,       // BluRay / Remux / WEB-DL / WEBRip / HDTV / DVD / CAM
    pub codec: Option<String>,        // x265 / x264 / AV1 / XviD
    pub audio: Option<String>,        // Atmos / TrueHD / DTS-HD / DTS / DDP / DD / AAC / FLAC
    pub hdr: Option<String>,          // DV / HDR10+ / HDR
    pub group: Option<String>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    pub year: Option<i32>,
    pub proper: bool,
    pub season_pack: bool,
    /// title with dots/underscores as spaces, tags stripped — used for dedupe keys
    pub clean_title: String,
}

fn find_any(hay: &str, needles: &[(&str, &str)]) -> Option<String> {
    for (pat, label) in needles {
        if hay.contains(pat) {
            return Some((*label).to_string());
        }
    }
    None
}

pub fn parse(title: &str) -> ParsedRelease {
    let mut p = ParsedRelease::default();
    // Normalize separators, keep original case for group detection.
    let norm: String = title
        .chars()
        .map(|c| match c {
            '.' | '_' => ' ',
            _ => c,
        })
        .collect();
    let upper = norm.to_uppercase();
    let u = upper.as_str();

    p.resolution = find_any(
        u,
        &[
            ("2160P", "2160p"), ("4K", "2160p"), ("UHD", "2160p"),
            ("1080P", "1080p"), ("1080I", "1080p"),
            ("720P", "720p"),
            ("576P", "576p"), ("480P", "480p"),
        ],
    );

    p.source = find_any(
        u,
        &[
            ("REMUX", "Remux"),
            ("BLURAY", "BluRay"), ("BLU-RAY", "BluRay"), ("BDRIP", "BluRay"), ("BRRIP", "BluRay"),
            ("WEB-DL", "WEB-DL"), ("WEBDL", "WEB-DL"),
            ("WEBRIP", "WEBRip"), ("WEB RIP", "WEBRip"),
            (" WEB ", "WEB-DL"),
            ("HDTV", "HDTV"), ("PDTV", "HDTV"), ("SDTV", "HDTV"),
            ("DVDRIP", "DVD"), ("DVD5", "DVD"), ("DVD9", "DVD"), ("DVDR", "DVD"),
            ("HDCAM", "CAM"), ("CAMRIP", "CAM"), (" CAM ", "CAM"),
            ("TELESYNC", "TS"), (" HDTS", "TS"), (" TS ", "TS"), ("TELECINE", "TC"),
            ("SCREENER", "SCR"), ("DVDSCR", "SCR"),
        ],
    );

    p.codec = find_any(
        u,
        &[
            ("X265", "x265"), ("H265", "x265"), ("H 265", "x265"), ("HEVC", "x265"),
            ("AV1", "AV1"),
            ("X264", "x264"), ("H264", "x264"), ("H 264", "x264"), ("AVC", "x264"),
            ("XVID", "XviD"), ("DIVX", "XviD"),
        ],
    );

    p.audio = find_any(
        u,
        &[
            ("ATMOS", "Atmos"),
            ("TRUEHD", "TrueHD"),
            ("DTS-HD", "DTS-HD"), ("DTS HD", "DTS-HD"), ("DTS-X", "DTS:X"), ("DTSX", "DTS:X"),
            ("DTS", "DTS"),
            ("DDP", "DDP"), ("DD+", "DDP"), ("EAC3", "DDP"), ("E-AC3", "DDP"), ("E-AC-3", "DDP"),
            ("DD5", "DD"), ("DD2", "DD"), ("AC3", "DD"), ("AC-3", "DD"),
            ("FLAC", "FLAC"),
            ("AAC", "AAC"),
            ("OPUS", "Opus"),
            ("MP3", "MP3"),
        ],
    );

    p.hdr = find_any(
        u,
        &[
            ("DOLBY VISION", "DV"), (" DV ", "DV"), ("DOVI", "DV"),
            ("HDR10+", "HDR10+"), ("HDR10PLUS", "HDR10+"),
            ("HDR10", "HDR"), (" HDR ", "HDR"), ("HLG", "HDR"),
        ],
    );

    p.proper = u.contains("PROPER") || u.contains("REPACK") || u.contains("RERIP");

    // Season / episode. Try SxxEyy first, then NxM, then bare season markers.
    let bytes = u.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'S' && i + 2 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() && j - i <= 2 {
                j += 1;
            }
            let season: i32 = u[i + 1..j].parse().unwrap_or(-1);
            if season >= 0 && p.season.is_none() {
                p.season = Some(season);
                if j < bytes.len() && bytes[j] == b'E' {
                    let mut k = j + 1;
                    while k < bytes.len() && bytes[k].is_ascii_digit() && k - j <= 3 {
                        k += 1;
                    }
                    if k > j + 1 {
                        p.episode = u[j + 1..k].parse().ok();
                    }
                }
            }
        }
        i += 1;
    }
    if p.season.is_none() {
        // "3x07" pattern
        let chars: Vec<char> = u.chars().collect();
        for w in 0..chars.len().saturating_sub(3) {
            if chars[w].is_ascii_digit()
                && chars[w + 1] == 'X'
                && chars[w + 2].is_ascii_digit()
                && (w == 0 || !chars[w - 1].is_ascii_alphanumeric())
            {
                p.season = chars[w].to_digit(10).map(|d| d as i32);
                let mut ep = String::new();
                let mut k = w + 2;
                while k < chars.len() && chars[k].is_ascii_digit() {
                    ep.push(chars[k]);
                    k += 1;
                }
                p.episode = ep.parse().ok();
                break;
            }
        }
    }
    if p.season.is_some() && p.episode.is_none() {
        p.season_pack = true;
    }
    if u.contains("COMPLETE") || u.contains("SEASON PACK") {
        p.season_pack = true;
    }

    // Year: standalone 19xx/20xx
    let chars: Vec<char> = norm.chars().collect();
    for w in 0..chars.len().saturating_sub(3) {
        let boundary_l = w == 0 || !chars[w - 1].is_ascii_alphanumeric();
        let boundary_r = w + 4 >= chars.len() || !chars[w + 4].is_ascii_alphanumeric();
        if boundary_l && boundary_r && chars[w..w + 4].iter().all(|c| c.is_ascii_digit()) {
            let year: i32 = chars[w..w + 4].iter().collect::<String>().parse().unwrap_or(0);
            if (1930..=2030).contains(&year) {
                p.year = Some(year);
                // keep first plausible year (usually the title year)
                break;
            }
        }
    }

    // Group: text after the last '-' if it looks like a tag (no spaces, short).
    if let Some(idx) = norm.rfind('-') {
        let tail = norm[idx + 1..].trim();
        let tail = tail.split('[').next().unwrap_or(tail).trim(); // strip "[rartv]" style suffixes
        if !tail.is_empty()
            && tail.len() <= 20
            && !tail.contains(' ')
            && tail.chars().all(|c| c.is_ascii_alphanumeric())
            && !tail.chars().all(|c| c.is_ascii_digit())
        {
            p.group = Some(tail.to_string());
        }
    }

    // Clean title: cut at the first structural marker.
    let cut_markers = [
        " S0", " S1", " S2", " S3", " SEASON ", " 2160P", " 1080P", " 720P", " 480P",
        " BLURAY", " BDRIP", " WEB-DL", " WEBDL", " WEBRIP", " HDTV", " DVDRIP", " REMUX",
        " X264", " X265", " H264", " H265", " HEVC", " AV1", " COMPLETE",
    ];
    let mut cut = upper.len();
    for m in cut_markers {
        if let Some(idx) = upper.find(m) {
            cut = cut.min(idx);
        }
    }
    // also cut before "(2024)"-style or bare year if it appears after some text
    if let Some(y) = p.year {
        if let Some(idx) = upper.find(&y.to_string()) {
            if idx > 3 {
                cut = cut.min(idx);
            }
        }
    }
    p.clean_title = norm[..cut]
        .trim()
        .trim_matches(['(', '[', '-', ' '])
        .to_string();
    if p.clean_title.is_empty() {
        p.clean_title = norm.trim().to_string();
    }

    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_tv() {
        let p = parse("The.Expanse.S02E05.1080p.WEB-DL.DDP5.1.H.264-NTb");
        assert_eq!(p.resolution.as_deref(), Some("1080p"));
        assert_eq!(p.source.as_deref(), Some("WEB-DL"));
        assert_eq!(p.codec.as_deref(), Some("x264"));
        assert_eq!(p.audio.as_deref(), Some("DDP"));
        assert_eq!(p.season, Some(2));
        assert_eq!(p.episode, Some(5));
        assert_eq!(p.group.as_deref(), Some("NTb"));
        assert_eq!(p.clean_title, "The Expanse");
    }

    #[test]
    fn parses_movie_with_year() {
        let p = parse("Dune.Part.Two.2024.2160p.UHD.BluRay.REMUX.DV.HDR10.TrueHD.Atmos.7.1-FraMeSToR");
        assert_eq!(p.resolution.as_deref(), Some("2160p"));
        assert_eq!(p.source.as_deref(), Some("Remux"));
        assert_eq!(p.year, Some(2024));
        assert_eq!(p.hdr.as_deref(), Some("DV"));
        assert_eq!(p.audio.as_deref(), Some("Atmos"));
        assert_eq!(p.clean_title, "Dune Part Two");
    }

    #[test]
    fn season_pack() {
        let p = parse("Severance S01 COMPLETE 2160p ATVP WEB-DL DDP5.1 HDR H.265-MIXED");
        assert_eq!(p.season, Some(1));
        assert!(p.season_pack);
        assert_eq!(p.episode, None);
    }
}
