//! Team mode: one member's attempts per tick, rotating through a CSV of
//! `nickname,email`, so attempts land at all hours, including the quiet ones.

use crate::app::{self, RaceArgs};
use crate::web::{clean_email, clean_nickname};
use crate::{report, say};
use anyhow::{Result, bail};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, PartialEq)]
pub struct Member {
    pub nickname: String,
    pub email: String,
}

/// `nickname,email` per line. Blank lines, `#` comments and a
/// `nickname,email` header are skipped.
pub fn parse(csv: &str) -> Result<Vec<Member>> {
    let mut members = Vec::new();
    for (index, line) in csv.lines().enumerate() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((nickname, email)) = line.rsplit_once(',') else {
            bail!("line {}: expected nickname,email", index + 1);
        };
        if nickname.trim().eq_ignore_ascii_case("nickname") {
            continue;
        }
        let nickname = clean_nickname(nickname.trim().trim_matches('"'))
            .map_err(|e| anyhow::anyhow!("line {}: {e}", index + 1))?;
        let email = clean_email(email.trim().trim_matches('"'))
            .map_err(|e| anyhow::anyhow!("line {}: {e}", index + 1))?;
        members.push(Member { nickname, email });
    }
    if members.is_empty() {
        bail!("no team member in the CSV");
    }
    Ok(members)
}

/// Never returns on its own. The first turn starts right away; the next ones
/// start one interval after the previous start, whatever the outcome. A turn
/// runs `attempts` races back to back for the same member.
pub async fn run(
    args: &RaceArgs,
    csv: &Path,
    every: Duration,
    attempts: u8,
    start: usize,
) -> Result<()> {
    let members = parse(&std::fs::read_to_string(csv)?)?;
    say!(
        "team mode: {} members, {attempts} attempts every {} min",
        members.len(),
        every.as_secs() / 60
    );
    let mut ticks = tokio::time::interval(every);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    for turn in start.. {
        ticks.tick().await;
        let member = &members[turn % members.len()];
        let mut race_args = args.clone();
        race_args.email = Some(member.email.clone());
        race_args.nickname = Some(member.nickname.clone());
        for attempt in 1..=attempts {
            report::clear();
            say!(
                "turn {turn}, attempt {attempt}/{attempts}: {} <{}>",
                member.nickname,
                member.email
            );
            if let Err(error) = app::run_race(&race_args).await {
                say!("turn {turn}, attempt {attempt}/{attempts} failed: {error:#}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_members_and_skips_header_and_comments() {
        let members = parse(
            "\u{feff}nickname,email\n# off this week\n\nAda, ada@example.com\n\"Bob, Jr\",bob@example.com\n",
        )
        .unwrap();
        assert_eq!(
            members,
            [
                Member {
                    nickname: "Ada".into(),
                    email: "ada@example.com".into()
                },
                Member {
                    nickname: "Bob, Jr".into(),
                    email: "bob@example.com".into()
                },
            ]
        );
    }

    #[test]
    fn rejects_bad_lines_and_empty_files() {
        assert!(parse("Ada ada@example.com").is_err());
        assert!(parse("Ada,not-an-email").is_err());
        assert!(parse(",ada@example.com").is_err());
        assert!(parse("nickname,email\n").is_err());
    }
}
