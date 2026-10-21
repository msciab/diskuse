use crate::scan::{Node, human};
use std::fmt::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// How many levels of subdirectories are listed under each top-level directory.
const MAX_DEPTH: usize = 3;

/// Today's date in UTC as (year, month, day).
pub fn today() -> (i64, u32, u32) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    civil_from_days((secs / 86400) as i64)
}

/// Converts days since 1970-01-01 to a calendar date (Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn count_dirs(node: &Node) -> u64 {
    1 + node.children.iter().map(count_dirs).sum::<u64>()
}

fn count_files(node: &Node) -> u64 {
    node.files_count + node.children.iter().map(count_files).sum::<u64>()
}

fn percent(size: u64, of: u64) -> String {
    if of == 0 {
        "0.0%".to_string()
    } else {
        format!("{:.1}%", size as f64 * 100.0 / of as f64)
    }
}

/// Formats a path as inline code, safe inside a table cell.
fn cell(path: &str) -> String {
    format!("`{}`", path.replace('|', "\\|").replace('`', "'"))
}

fn row(out: &mut String, path: &str, size: u64, parent: u64, total: u64) {
    let _ = writeln!(
        out,
        "| {} | {} | {} | {} |",
        cell(path),
        human(size),
        percent(size, parent),
        percent(size, total)
    );
}

const HEADER: &str = "| Path | Size | Relative % | Absolute % |\n| --- | ---: | ---: | ---: |\n";

/// Lists the subdirectories of `node` (at relative path `rel`) down to
/// `MAX_DEPTH` levels below the top-level directory.
fn subtree_rows(
    out: &mut String,
    node: &Node,
    rel: &str,
    depth: usize,
    total: u64,
    visible: &impl Fn(u64) -> bool,
) {
    if depth > MAX_DEPTH {
        return;
    }
    // Children are sorted by size, so the visible ones come first.
    for child in node.children.iter().take_while(|c| visible(c.size)) {
        let child_rel = format!("{rel}/{}", child.name);
        row(out, &child_rel, child.size, node.size, total);
        subtree_rows(out, child, &child_rel, depth + 1, total, visible);
    }
}

/// Renders the scanned tree as a Markdown report. Directories not bigger
/// than `min_percent` of the whole tree are left out (0 keeps everything).
pub fn render(root: &Node, root_path: &Path, min_percent: f64) -> String {
    let total = root.size;
    let visible =
        |size: u64| min_percent <= 0.0 || size as f64 > total as f64 * min_percent / 100.0;
    let shown = root.children.iter().take_while(|c| visible(c.size));
    let (y, m, d) = today();
    let mut out = String::new();

    let _ = writeln!(out, "# Disk usage report\n");
    let _ = writeln!(out, "- **Path:** `{}`", root_path.display());
    let _ = writeln!(out, "- **Date:** {y:04}-{m:02}-{d:02} (UTC)");
    let _ = writeln!(out, "- **Total size:** {}", human(total));
    let _ = writeln!(out, "- **Directories:** {}", count_dirs(root));
    let _ = writeln!(out, "- **Files:** {}", count_files(root));
    let _ = writeln!(out, "- **Top-level directories:** {}", root.children.len());
    if min_percent > 0.0 {
        let _ = writeln!(
            out,
            "- **Filter:** directories of {min_percent}% of the total or less are not listed"
        );
    }
    out.push('\n');

    let _ = writeln!(out, "## Top-level\n");
    out.push_str(HEADER);
    for child in shown.clone() {
        row(&mut out, &child.name, child.size, total, total);
    }
    if root.files_count > 0 {
        let label = format!("(files in root: {})", root.files_count);
        row(&mut out, &label, root.files_size, total, total);
    }

    for child in shown {
        let _ = writeln!(out, "\n## {}\n", child.name.replace('|', "\\|"));
        out.push_str(HEADER);
        row(&mut out, &child.name, child.size, total, total);
        subtree_rows(&mut out, child, &child.name, 1, total, &visible);
    }
    out
}
