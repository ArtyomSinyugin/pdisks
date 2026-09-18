use std::collections::HashMap;

/// Parsed MD RAID information from /proc/mdstat.
#[derive(Debug, Default)]
pub struct MdInfo {
    /// Arrays: name -> ArrayInfo
    pub arrays: HashMap<String, ArrayInfo>,
}

#[derive(Debug)]
pub struct ArrayInfo {
    pub name: String,
    pub level: String,
    pub total_devices: usize,
    pub active_devices: usize,
    pub members: Vec<String>,
    pub state: MdState,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MdState {
    Clean,
    Resyncing,
    Degraded,
    Inconsistent,
}

/// Parse /proc/mdstat content.
pub fn parse_mdstat(content: &str) -> MdInfo {
    let mut info = MdInfo::default();

    let lines: Vec<&str> = content.lines().collect();
    for (idx, raw_line) in lines.iter().enumerate() {
        let line = raw_line.trim();

        // Array definition line: "md0 : active raid1 sdb1[1] sda1[0]".
        // Skip non-array lines ("Personalities : [raid1]", "bitmap: ...") by
        // requiring the details to start with a state keyword.
        if line.contains(':') && !line.starts_with(' ') {
            let parts: Vec<&str> = line.splitn(2, ':').collect();
            if parts.len() == 2 {
                let array_name = parts[0].trim().to_string();
                let details = parts[1].trim();

                // Only real arrays have a state keyword ("active", "inactive")
                // as the first token of the details.
                let is_array = matches!(
                    details.split_whitespace().next(),
                    Some("active" | "inactive")
                );
                if !is_array {
                    continue;
                }

                // The "[2/2]" count lives on the indented detail line(s) that
                // follow this definition line.
                let mut tail: Vec<&str> = lines[idx + 1..]
                    .iter()
                    .take_while(|l| l.trim().is_empty() || l.starts_with(' '))
                    .copied()
                    .collect();
                tail.push(details);
                let tail_text = tail.join("\n");

                if let Some(array) = parse_array_line(&array_name, details, &tail_text) {
                    info.arrays.insert(array_name, array);
                }
            }
        }
    }

    info
}

fn parse_array_line(name: &str, details: &str, tail_text: &str) -> Option<ArrayInfo> {
    // Expected format: "active raid1 sdb1[1] sda1[0]"
    let tokens: Vec<&str> = details.split_whitespace().collect();

    if tokens.len() < 2 {
        return None;
    }

    let state_str = tokens[0];
    let level = tokens[1].to_string();

    // Extract member devices (tokens like "sdb1[1]")
    let mut members = Vec::new();
    for token in &tokens[2..] {
        if let Some(bracket_pos) = token.find('[') {
            let device = &token[..bracket_pos];
            members.push(device.to_string());
        }
    }

    // The "[2/2]" count lives on the indented detail line that follows the
    // array definition line.
    let (total, active) = extract_device_counts(tail_text);

    Some(ArrayInfo {
        name: name.to_string(),
        level,
        total_devices: total,
        active_devices: active,
        members,
        state: if state_str == "active" && active < total {
            MdState::Degraded
        } else if details.contains("resync") || details.contains("recovery") {
            MdState::Resyncing
        } else {
            MdState::Clean
        },
    })
}

fn extract_device_counts(content: &str) -> (usize, usize) {
    // Look for a "[2/2]" / "[2/1]" pattern — the bracket pair whose inner text
    // is exactly two numbers separated by '/'. Member indices like "sda1[0]"
    // contain a single number and must be skipped.
    let mut start = 0;
    while let Some(rel) = content[start..].find('[') {
        let open = start + rel;
        if let Some(rel_close) = content[open + 1..].find(']') {
            let close = open + 1 + rel_close;
            let inner = &content[open + 1..close];
            let parts: Vec<&str> = inner.split('/').collect();
            if parts.len() == 2
                && !parts[0].is_empty()
                && !parts[1].is_empty()
                && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit()))
            {
                let total = parts[0].parse::<usize>().unwrap_or(0);
                let active = parts[1].parse::<usize>().unwrap_or(0);
                return (total, active);
            }
        }
        start = open + 1;
    }

    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_raid1() {
        let mdstat = r#"Personalities : [raid1]
md0 : active raid1 sdb1[1] sda1[0]
      214746752 blocks super 1.2 [2/2] [UU]

bitmap: 0/8 pages [0/8 KB], 65536k chunk"#;

        let info = parse_mdstat(mdstat);
        assert_eq!(info.arrays.len(), 1);

        let array = &info.arrays["md0"];
        assert_eq!(array.level, "raid1");
        assert_eq!(array.total_devices, 2);
        assert_eq!(array.active_devices, 2);
        assert_eq!(array.members, vec!["sdb1", "sda1"]);
        assert_eq!(array.state, MdState::Clean);
    }

    #[test]
    fn parse_degraded_raid1() {
        let mdstat = r#"Personalities : [raid1]
md0 : active raid1 sda2[0]
      499865088 blocks super 1.2 [2/1] [_U]
      resync = P(100.0%) speed=300000K/sec

bitmap: 0/8 pages [0/8 KB], 65536k chunk"#;

        let info = parse_mdstat(mdstat);
        assert_eq!(info.arrays.len(), 1);

        let array = &info.arrays["md0"];
        assert_eq!(array.total_devices, 2);
        assert_eq!(array.active_devices, 1);
        assert_eq!(array.state, MdState::Degraded);
    }
}
