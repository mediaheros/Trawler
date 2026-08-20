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
    // ASCII-only uppercase: to_uppercase() is NOT length-preserving ('ı'→'I'
    // shrinks), and every index below is a byte offset into `norm` — mixing
    // the two index spaces panicked (and with panic=abort, killed the app)
    // on Turkish titles. char::to_ascii_uppercase keeps byte parity exactly.
    let upper: String = norm.chars().map(|c| c.to_ascii_uppercase()).collect();
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
    // where the matched season marker ends — everything after it is release
    // tags, where supplemental markers (EXTRAS, …) actually mean something
    let mut marker_end: Option<usize> = None;
    while i < bytes.len() {
        // 'S' must start a word — otherwise "DTS5.1" reads as season 5
        let word_start = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        if word_start && bytes[i] == b'S' && i + 2 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() && j - i <= 2 {
                j += 1;
            }
            // a longer digit run ("S2023") is a year or id, not a season
            if j < bytes.len() && bytes[j].is_ascii_digit() {
                i = j;
                continue;
            }
            let season: i32 = u[i + 1..j].parse().unwrap_or(-1);
            if season >= 0 && p.season.is_none() {
                p.season = Some(season);
                marker_end.get_or_insert(j);
                // allow one separator between season and episode: S01E05,
                // S01.E05 (dot became space), S01 E05, S01xE05, S01-E05
                let mut e = j;
                if e < bytes.len()
                    && matches!(bytes[e], b' ' | b'-' | b'X')
                    && e + 1 < bytes.len()
                    && bytes[e + 1] == b'E'
                {
                    e += 1;
                }
                if e < bytes.len() && bytes[e] == b'E' {
                    // "EP05" anime marker: an optional P sits between E and
                    // the digits — only when a digit actually follows
                    let digits_start = if e + 2 < bytes.len()
                        && bytes[e + 1] == b'P'
                        && bytes[e + 2].is_ascii_digit()
                    {
                        e + 2
                    } else {
                        e + 1
                    };
                    let mut k = digits_start;
                    while k < bytes.len() && bytes[k].is_ascii_digit() && k - digits_start <= 3 {
                        k += 1;
                    }
                    if k > digits_start {
                        p.episode = u[digits_start..k].parse().ok();
                    }
                }
            }
        }
        i += 1;
    }
    if p.season.is_none() {
        // "3x07" / "26x04" pattern — long-runners use multi-digit seasons.
        // The season cap and the right-hand boundary keep "1920x1080"
        // resolution strings from parsing as season 1920.
        let chars: Vec<char> = u.chars().collect();
        for w in 0..chars.len() {
            if !chars[w].is_ascii_digit() || !(w == 0 || !chars[w - 1].is_ascii_alphanumeric()) {
                continue;
            }
            let mut s_end = w;
            while s_end < chars.len() && chars[s_end].is_ascii_digit() {
                s_end += 1;
            }
            if s_end >= chars.len()
                || chars[s_end] != 'X'
                || s_end + 1 >= chars.len()
                || !chars[s_end + 1].is_ascii_digit()
            {
                continue;
            }
            let season: i32 = chars[w..s_end].iter().collect::<String>().parse().unwrap_or(-1);
            let mut k = s_end + 1;
            while k < chars.len() && chars[k].is_ascii_digit() {
                k += 1;
            }
            let right_boundary = k >= chars.len() || !chars[k].is_ascii_alphanumeric();
            if season > 0 && season < 100 && right_boundary {
                p.season = Some(season);
                p.episode = chars[s_end + 1..k].iter().collect::<String>().parse().ok();
                // k is a char index — convert to the byte offset the tail
                // slice needs, or a multibyte prefix panics mid-character
                let byte_end: usize = chars[..k].iter().map(|c| c.len_utf8()).sum();
                marker_end.get_or_insert(byte_end);
                break;
            }
        }
    }
    // A pack needs positive evidence. "Season present, episode missing" alone
    // used to be enough — which turned single episodes with unusual episode
    // markers into "packs" that stamped whole seasons as grabbed. Bare-season
    // titles ("Show S02 1080p WEB") only count when quality tags corroborate
    // that this is a real release listing — and supplemental releases
    // (extras, specials) are never packs, whatever tags they carry. The
    // supplemental check scans only the tags AFTER the season marker: a show
    // named "Special Ops" must not lose its packs to its own name.
    let supplemental = {
        let tail = &u[marker_end.unwrap_or(0)..];
        tail.split(|c: char| !c.is_ascii_alphanumeric()).any(|tok| {
            matches!(tok, "EXTRAS" | "SPECIAL" | "SPECIALS" | "OMAKE" | "BONUS")
        })
    };
    p.season_pack = !supplemental
        && (u.contains("COMPLETE")
            || u.contains("SEASON PACK")
            || u.contains("FULL SEASON")
            || (p.season.is_some()
                && p.episode.is_none()
                && (p.resolution.is_some() || p.source.is_some())));

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
        "(2160P", "(1080P", "(720P", "(480P", "[2160P", "[1080P", "[720P", "[480P",
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
    // belt and braces: `upper` is byte-parallel to `norm` by construction,
    // but a slice must never be able to panic on indexer-controlled input
    while cut > 0 && !norm.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut ct = norm[..cut].trim().trim_matches(['(', '[', '-', ' ']).to_string();
    // anime-style names: drop the leading "[Group]" tag, trailing bracketed
    // CRC/quality groups, and container extensions, so two groups' releases
    // of the same episode share one identity in the dedupe ledger
    if let Some(close) = ct.find(']') {
        if ct[..close].len() <= 24 && (ct.starts_with('[') || !ct[..close].contains(' ') || close < 24) {
            let rest = ct[close + 1..].trim();
            if !rest.is_empty() {
                ct = rest.to_string();
            }
        }
    }
    while let Some(open) = ct.rfind('[') {
        if ct[open..].contains(']') || ct.len() - open <= 16 {
            ct = ct[..open].trim_end().to_string();
        } else {
            break;
        }
    }
    for ext in [" mkv", " mp4", " avi", " MKV", " MP4", " AVI"] {
        if ct.ends_with(ext) {
            ct = ct[..ct.len() - ext.len()].trim_end().to_string();
        }
    }
    p.clean_title = ct.trim().trim_matches(['(', '[', ']', '-', ' ']).to_string();
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
        // bare season + quality tags is a pack listing
        assert!(parse("Show S02 1080p WEB-DL x265-GRP").season_pack);
    }

    #[test]
    fn multibyte_titles_never_panic_or_truncate() {
        // 'ı' uppercases to a shorter byte sequence — this used to abort the app
        let p = parse("Kızılcık.Şerbeti.S03E12.1080p.WEB-DL.x264-TR");
        assert_eq!(p.season, Some(3));
        assert_eq!(p.episode, Some(12));
        assert_eq!(p.clean_title, "Kızılcık Şerbeti");
        let p2 = parse("ı.S01E01.1080p");
        assert_eq!(p2.clean_title, "ı");
        parse("Bir Başkadır S01 COMPLETE 1080p");
        parse("ﬁnale.ß.S01E01.720p");
    }

    #[test]
    fn separated_episode_markers_are_episodes_not_packs() {
        let p = parse("Show.Name.S01.E05.1080p.WEB");
        assert_eq!((p.season, p.episode), (Some(1), Some(5)));
        assert!(!p.season_pack);
        let p2 = parse("Show Name S01xE05 720p HDTV");
        assert_eq!((p2.season, p2.episode), (Some(1), Some(5)));
    }

    #[test]
    fn no_false_seasons_from_words_or_years() {
        // "DTS5.1" must not become season 5
        let p = parse("Movie.Title.2024.1080p.BluRay.DTS5.1.x264-GRP");
        assert_eq!(p.season, None);
        assert!(!p.season_pack);
        // "S2023" is a year-like id, not season 20
        let p2 = parse("Show.Name.S2023E05.1080p.WEB");
        assert_eq!(p2.season, None);
    }

    #[test]
    fn anime_titles_share_identity_across_groups() {
        let a = parse("[SubsPlease] Frieren - 12 (1080p) [A1B2C3D4].mkv");
        let b = parse("[Erai-raws] Frieren - 12 [1080p][Multiple Subtitle]");
        assert_eq!(a.clean_title, "Frieren - 12");
        assert_eq!(a.clean_title, b.clean_title);
    }

    // ---- shakedown regression tests ----

    #[test]
    fn ep_prefixed_episodes_parse_as_episodes_not_packs() {
        let p = parse("Show.S01EP05.1080p.WEB-DL.x264-GRP");
        assert_eq!((p.season, p.episode), (Some(1), Some(5)), "EP05 is episode 5");
        assert!(!p.season_pack, "a single episode must never classify as a season pack");
    }

    #[test]
    fn unpadded_single_digit_episodes_still_parse() {
        let p = parse("Show.S01E5.720p.WEB-DL.x264-GRP");
        assert_eq!((p.season, p.episode), (Some(1), Some(5)));
        assert!(!p.season_pack);
    }

    #[test]
    fn supplemental_releases_are_not_season_packs() {
        for title in [
            "Show.S01-EXTRAS.1080p.WEB-DL.x264-GRP",
            "Show.S02.SPECIAL.1080p.BluRay.x264-GRP",
            "Show.S01.OMAKE.720p.WEB-DL.x264-GRP",
        ] {
            let p = parse(title);
            assert!(p.episode.is_none());
            assert!(!p.season_pack, "{title} must not stamp a whole season as grabbed");
        }
    }

    #[test]
    fn show_names_do_not_suppress_their_own_packs() {
        // the supplemental check must scan only tags after the season marker,
        // never the show name
        let a = parse("Special Ops Lioness S02 COMPLETE 1080p BluRay x265-RARBG");
        assert!(a.season_pack, "the show's name must not cost it its packs");
        let b = parse("Extras S01 COMPLETE DVDRip XviD-GROUP");
        assert!(b.season_pack);
    }

    #[test]
    fn multi_digit_nxm_seasons_parse() {
        let p = parse("Show.26x04.720p.WEB-DL.x264-GRP");
        assert_eq!((p.season, p.episode), (Some(26), Some(4)));
        let p = parse("Show.10x05.1080p.WEB-DL.x264-GRP");
        assert_eq!((p.season, p.episode), (Some(10), Some(5)));
    }

    #[test]
    fn resolution_strings_do_not_parse_as_nxm_seasons() {
        let p = parse("Movie.2023.1920x1080.Documentary-GROUP");
        assert_eq!((p.season, p.episode), (None, None), "1920x1080 is a resolution, not S1920E1080");
    }

    #[test]
    fn multibyte_titles_with_nxm_markers_do_not_panic() {
        // char-index vs byte-offset confusion here has killed the app on
        // Turkish titles before (panic = abort in release) — see file header
        let p = parse("ıııııııııı 3x07 1080p");
        assert_eq!((p.season, p.episode), (Some(3), Some(7)));
        let p2 = parse("Kızılcık Şerbeti 4x12.HDTV");
        assert_eq!((p2.season, p2.episode), (Some(4), Some(12)));
    }

    #[test]
    fn genuine_packs_still_classify_as_packs() {
        let a = parse("Show.S02.Complete.1080p.BluRay.x265-RARBG");
        assert!(a.season_pack);
        let b = parse("Show.S02.1080p.WEB-DL.x264-GRP");
        assert!(b.season_pack, "bare season with quality corroboration is a pack");
    }
}
