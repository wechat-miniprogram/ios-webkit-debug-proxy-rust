// Port configuration parsing module
// Corresponds to the original C code port_config.c / port_config.h

use regex::Regex;
use std::fs;
use std::io::{BufRead, BufReader};

/// Port configuration entry
#[derive(Debug, Clone)]
pub(crate) struct PortConfigEntry {
    /// Device ID (None means null/device list, Some("*") means wildcard)
    pub(crate) device_id: Option<String>,
    /// Minimum port
    pub(crate) min_port: i32,
    /// Maximum port
    pub(crate) max_port: i32,
}

/// Port configuration manager
pub(crate) struct PortConfig {
    entries: Vec<PortConfigEntry>,
    re: Regex,
}

impl PortConfig {
    pub(crate) fn new() -> Self {
        let re = Regex::new(
            r"(?i)^[ \t]*(([a-fA-F0-9-]{25,}|\*|null)[ \t]*:?|:)[ \t]*(-?[0-9]+)?([ \t]*-[ \t]*([0-9]+))?[ \t]*$"
        ).expect("Regex compilation failed");

        PortConfig {
            entries: Vec::new(),
            re,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    /// Add a rule
    pub(crate) fn add(&mut self, device_id: Option<String>, min_port: i32, max_port: i32) {
        self.entries.push(PortConfigEntry {
            device_id,
            min_port,
            max_port,
        });
    }

    /// Parse a line and add all rules
    /// Returns None on success, Some(position) for the first invalid item's position
    pub(crate) fn add_line(&mut self, line: &str) -> Option<usize> {
        let mut curr = 0;
        let bytes = line.as_bytes();
        let len = bytes.len();

        while curr < len {
            // Skip whitespace
            while curr < len && (bytes[curr] == b' ' || bytes[curr] == b'\t') {
                curr += 1;
            }

            // Find segment end
            let mut end = curr;
            while end < len
                && bytes[end] != 0
                && bytes[end] != b'\n'
                && bytes[end] != b'#'
                && bytes[end] != b','
            {
                end += 1;
            }

            if curr < end {
                let segment = &line[curr..end];
                if let Some(caps) = self.re.captures(segment) {
                    let device_id = if let Some(m) = caps.get(2) {
                        let val = m.as_str();
                        if val.eq_ignore_ascii_case("null") {
                            None
                        } else {
                            Some(val.to_string())
                        }
                    } else {
                        Some("*".to_string())
                    };

                    let min_port: i32 = if let Some(m) = caps.get(3) {
                        m.as_str().parse().unwrap_or(0)
                    } else {
                        0 // Empty port means auto-assigned by the system
                    };
                    let max_port = if let Some(m) = caps.get(5) {
                        m.as_str().parse().unwrap_or(min_port)
                    } else {
                        min_port
                    };

                    self.add(device_id, min_port, max_port);
                } else {
                    return Some(curr);
                }
            }

            if end >= len || bytes[end] != b',' {
                break;
            }
            curr = end + 1;
        }

        None
    }

    /// Add rules from a file
    pub(crate) fn add_file(&mut self, filename: &str) -> Result<(), String> {
        let file =
            fs::File::open(filename).map_err(|e| format!("Cannot open file {}: {}", filename, e))?;
        let reader = BufReader::new(file);

        for (line_num, line_result) in reader.lines().enumerate() {
            let line = line_result.map_err(|e| format!("Failed to read file: {}", e))?;
            if let Some(_pos) = self.add_line(&line) {
                eprintln!("Ignoring {}:{}: {}", filename, line_num, line);
            }
        }

        Ok(())
    }

    /// Find a matching configuration entry
    pub(crate) fn find(&self, device_id: Option<&str>) -> Option<&PortConfigEntry> {
        for entry in &self.entries {
            match (&entry.device_id, device_id) {
                (Some(s), _) if s == "*" => return Some(entry),
                (Some(s), Some(did)) if s.eq_ignore_ascii_case(did) => return Some(entry),
                (None, None) => return Some(entry),
                _ => continue,
            }
        }
        None
    }

    /// Select port
    pub(crate) fn select_port(
        &self,
        device_id: Option<&str>,
        current_port: &mut i32,
        min_port: &mut i32,
        max_port: &mut i32,
    ) -> Result<(), ()> {
        if let Some(config) = self.find(device_id) {
            *min_port = config.min_port;
            *max_port = config.max_port;
            if *current_port >= 0 && (*current_port < *min_port || *current_port > *max_port) {
                *current_port = -1;
            }
            Ok(())
        } else {
            *min_port = -1;
            *max_port = -1;
            *current_port = -1;
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config_line() {
        let mut pc = PortConfig::new();
        assert!(pc.add_line("null:9221,:9222-9322").is_none());
        assert_eq!(pc.entries.len(), 2);
        assert!(pc.entries[0].device_id.is_none());
        assert_eq!(pc.entries[0].min_port, 9221);
        assert_eq!(pc.entries[1].device_id, Some("*".to_string()));
        assert_eq!(pc.entries[1].min_port, 9222);
        assert_eq!(pc.entries[1].max_port, 9322);
    }

    #[test]
    fn test_parse_config_empty_port() {
        // Empty after colon means port is auto-assigned by the system
        let mut pc = PortConfig::new();
        assert!(pc.add_line("null:9221,:").is_none());
        assert_eq!(pc.entries.len(), 2);
        assert!(pc.entries[0].device_id.is_none());
        assert_eq!(pc.entries[0].min_port, 9221);
        // Wildcard entry port is 0, meaning auto-assigned by the system
        assert_eq!(pc.entries[1].device_id, Some("*".to_string()));
        assert_eq!(pc.entries[1].min_port, 0);
        assert_eq!(pc.entries[1].max_port, 0);
    }
}
