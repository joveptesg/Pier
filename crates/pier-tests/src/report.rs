//! Output formatters: human-friendly Markdown + machine-readable JUnit XML.

use crate::scenario::{ScenarioResult, Status};

pub struct Report {
    suite: String,
    pier_version: String,
    results: Vec<ScenarioResult>,
}

impl Report {
    pub fn new(suite: impl Into<String>, pier_version: impl Into<String>) -> Self {
        Self {
            suite: suite.into(),
            pier_version: pier_version.into(),
            results: Vec::new(),
        }
    }

    pub fn push(&mut self, r: ScenarioResult) {
        let icon = match r.status {
            Status::Pass => "✓",
            Status::Fail => "✗",
            Status::Skipped => "○",
        };
        let notes = if r.notes.is_empty() {
            String::new()
        } else {
            format!(" — {}", r.notes)
        };
        tracing::info!("{icon} {} ({} ms){}", r.name, r.duration_ms, notes);
        self.results.push(r);
    }

    pub fn failed(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.status == Status::Fail)
            .count()
    }

    pub fn to_markdown(&self) -> String {
        let total = self.results.len();
        let passed = self
            .results
            .iter()
            .filter(|r| r.status == Status::Pass)
            .count();
        let failed = self.failed();
        let skipped = self
            .results
            .iter()
            .filter(|r| r.status == Status::Skipped)
            .count();

        let mut s = String::new();
        s.push_str(&format!(
            "# {} — Pier {}\n\n",
            self.suite, self.pier_version
        ));
        s.push_str(&format!(
            "Date: {}\n\nTotal: {total} · PASS: {passed} · FAIL: {failed} · SKIPPED: {skipped}\n\n",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
        ));
        s.push_str("| Scenario | Status | Duration | Notes |\n");
        s.push_str("|---|---|---|---|\n");
        for r in &self.results {
            let icon = match r.status {
                Status::Pass => "✓",
                Status::Fail => "✗",
                Status::Skipped => "○",
            };
            s.push_str(&format!(
                "| `{}` | {} | {} ms | {} |\n",
                r.name, icon, r.duration_ms, r.notes
            ));
        }
        s
    }

    pub fn to_junit(&self) -> String {
        let total = self.results.len();
        let failed = self.failed();
        let skipped = self
            .results
            .iter()
            .filter(|r| r.status == Status::Skipped)
            .count();
        let total_time_s = self
            .results
            .iter()
            .map(|r| r.duration_ms as f64 / 1000.0)
            .sum::<f64>();

        let mut s = String::new();
        s.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        s.push('\n');
        s.push_str(&format!(
            r#"<testsuite name="{}" tests="{}" failures="{}" skipped="{}" time="{:.3}">"#,
            xml_escape(&self.suite),
            total,
            failed,
            skipped,
            total_time_s
        ));
        s.push('\n');
        for r in &self.results {
            let t = r.duration_ms as f64 / 1000.0;
            s.push_str(&format!(
                r#"  <testcase classname="pier-tests" name="{}" time="{:.3}">"#,
                xml_escape(&r.name),
                t
            ));
            s.push('\n');
            match r.status {
                Status::Fail => {
                    s.push_str(&format!(
                        r#"    <failure message="{}"/>"#,
                        xml_escape(&r.notes)
                    ));
                    s.push('\n');
                }
                Status::Skipped => {
                    s.push_str("    <skipped/>\n");
                }
                Status::Pass => {}
            }
            s.push_str("  </testcase>\n");
        }
        s.push_str("</testsuite>\n");
        s
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
