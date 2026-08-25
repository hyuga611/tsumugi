//! AI エージェントに「自分の状態を名乗らせる」ための配線。
//!
//! **推測しない。** 画面を読んで「いま返事待ちだろう」と当てにいく実装は、
//! 相手が出力の形を変えた日に黙って壊れる。壊れたことにも気づけない。
//! だから当てにいかず、エージェント自身が持っているフックの口から
//! `tsg --agent-state <状態>` を呼ばせる。
//!
//! 触るのは**ユーザの設定ファイル**なので、次を守る。
//!
//! - 既にある設定を消さない（同じ配列に足すだけ）
//! - 2 回入れても増えない（同じ行があれば足さない）
//! - `--uninstall-agent-hooks` で足した分だけ消える
//! - 何を書き換えたかを毎回画面に出す

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::install::Report;

/// 対応しているエージェント。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    fn parse(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "cc" => Self::Claude,
            "codex" => Self::Codex,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }
}

/// 名前が無ければ、入っているものを全部。
fn targets(name: Option<&str>) -> Result<Vec<Agent>> {
    match name {
        Some(n) => match Agent::parse(n) {
            Some(a) => Ok(vec![a]),
            None => bail!("'{n}' を知りません（claude / codex）"),
        },
        None => Ok([Agent::Claude, Agent::Codex]
            .into_iter()
            .filter(|a| settings_path(*a).is_some_and(|p| p.parent().is_some_and(|d| d.exists())))
            .collect()),
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn settings_path(a: Agent) -> Option<PathBuf> {
    let h = home()?;
    Some(match a {
        Agent::Claude => h.join(".claude").join("settings.json"),
        Agent::Codex => h.join(".codex").join("hooks.json"),
    })
}

/// どのフックがどの状態を意味するか。
///
/// `Stop` は「エージェントが喋り終わった」＝**人の番**。`Notification` は
/// 許可を求めて止まっている＝やはり人の番。この 2 つが要点で、
/// 残りは「動いている / 何もしていない」へ戻すためにある。
fn events(a: Agent) -> &'static [(&'static str, &'static str)] {
    match a {
        Agent::Claude => &[
            ("SessionStart", "idle"),
            ("UserPromptSubmit", "working"),
            ("Notification", "blocked"),
            ("Stop", "done"),
        ],
        Agent::Codex => &[
            ("session_start", "idle"),
            ("user_prompt", "working"),
            ("turn_end", "done"),
        ],
    }
}

/// 呼ばせる 1 行。`tsg` は PATH に居る前提（`--install` が入れる）。
fn command_for(state: &str) -> String {
    format!("tsg --agent-state {state}")
}

/// 「いくら使ったか」も名乗らせたいときの形。人が設定に書き足す用の見本。
///
/// **こちらでは数えない。** 数え方はモデルごとに違い、当てにいくと必ずずれる。
/// 相手が言った数字をそのまま出す。
pub fn cost_example() -> &'static str {
    "tsg --agent-state done --cost \"$0.42\""
}

/// 足す。
pub fn install(name: Option<&str>) -> Result<Report> {
    let mut report = Report::default();
    for a in targets(name)? {
        let Some(path) = settings_path(a) else {
            report.notes.push("設定の置き場所が分かりません".into());
            continue;
        };
        let mut root = read_json(&path)?;
        let mut added = 0usize;
        for (event, state) in events(a) {
            if add_hook(&mut root, event, &command_for(state)) {
                added += 1;
            }
        }
        if added == 0 {
            report
                .done
                .push(format!("{} には既に入っていました", a.label()));
            continue;
        }
        write_json(&path, &root)?;
        report.done.push(format!(
            "{} に {added} 個のフックを足した: {}",
            a.label(),
            path.display()
        ));
    }
    if report.done.is_empty() {
        report
            .notes
            .push("入っているエージェントが見つかりませんでした（claude / codex）".into());
    } else {
        report
            .notes
            .push("外すときは `tsg --uninstall-agent-hooks`".into());
        report.notes.push(format!(
            "「いくら使ったか」も出したいときは、設定に {} のような行を足す",
            cost_example()
        ));
    }
    Ok(report)
}

/// 外す。**足した分だけ**消す。
pub fn uninstall(name: Option<&str>) -> Result<Report> {
    let mut report = Report::default();
    for a in targets(name)? {
        let Some(path) = settings_path(a) else {
            continue;
        };
        if !path.exists() {
            continue;
        }
        let mut root = read_json(&path)?;
        let mut removed = 0usize;
        for (event, state) in events(a) {
            if remove_hook(&mut root, event, &command_for(state)) {
                removed += 1;
            }
        }
        if removed > 0 {
            write_json(&path, &root)?;
            report
                .done
                .push(format!("{} から {removed} 個外した", a.label()));
        }
    }
    if report.done.is_empty() {
        report.notes.push("外すものはありませんでした".into());
    }
    Ok(report)
}

fn read_json(path: &std::path::Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("{} を読めません", path.display()))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    // **読めない設定を上書きしない。** 人が手で書いたものを壊すと戻せない。
    serde_json::from_str(&text)
        .with_context(|| format!("{} が JSON として読めません", path.display()))
}

fn write_json(path: &std::path::Path, root: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(root)?;
    std::fs::write(path, text + "\n").with_context(|| format!("{} を書けません", path.display()))
}

/// `hooks.<event>[].hooks[]` に 1 つ足す。既にあれば何もしない。
///
/// 形は Claude Code の設定に合わせてある。既存の入れ子を作り替えず、
/// **自分の 1 個だけを持つ組**を末尾に足す。人が書いた `matcher` の中へ
/// 紛れ込ませない。
fn add_hook(root: &mut Value, event: &str, command: &str) -> bool {
    if has_hook(root, event, command) {
        return false;
    }
    if !root.is_object() {
        return false;
    }
    let hooks = root
        .as_object_mut()
        .expect("object であることは上で確かめた")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let Some(map) = hooks.as_object_mut() else {
        return false;
    };
    let Some(list) = map.entry(event).or_insert_with(|| json!([])).as_array_mut() else {
        return false;
    };
    list.push(json!({ "hooks": [ { "type": "command", "command": command } ] }));
    true
}

fn has_hook(root: &Value, event: &str, command: &str) -> bool {
    commands_of(root, event).any(|c| c == command)
}

fn commands_of<'a>(root: &'a Value, event: &str) -> impl Iterator<Item = &'a str> {
    root.get("hooks")
        .and_then(|h| h.get(event))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|g| g.get("hooks"))
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|h| h.get("command"))
        .filter_map(Value::as_str)
}

/// 足したものだけ消す。空になった入れ物も片付ける。
fn remove_hook(root: &mut Value, event: &str, command: &str) -> bool {
    let Some(groups) = root
        .get_mut("hooks")
        .and_then(|h| h.get_mut(event))
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let mut hit = false;
    for g in groups.iter_mut() {
        if let Some(list) = g.get_mut("hooks").and_then(Value::as_array_mut) {
            let before = list.len();
            list.retain(|h| h.get("command").and_then(Value::as_str) != Some(command));
            hit |= list.len() != before;
        }
    }
    // 自分のせいで空になった組だけ落とす（元から空だったものは触らない）
    if hit {
        groups.retain(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|l| !l.is_empty())
        });
        if groups.is_empty()
            && let Some(h) = root.get_mut("hooks").and_then(Value::as_object_mut)
        {
            h.remove(event);
        }
    }
    hit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adding_a_hook_keeps_what_was_already_there() {
        let mut root = json!({
            "model": "opus",
            "hooks": {
                "SessionStart": [
                    { "hooks": [ { "type": "command", "command": "narai-start" } ] }
                ]
            }
        });
        assert!(add_hook(
            &mut root,
            "SessionStart",
            "tsg --agent-state idle"
        ));
        let got: Vec<&str> = commands_of(&root, "SessionStart").collect();
        assert_eq!(got, vec!["narai-start", "tsg --agent-state idle"]);
        assert_eq!(root["model"], "opus", "無関係な設定を触った");
    }

    #[test]
    fn adding_twice_does_not_duplicate() {
        let mut root = json!({});
        assert!(add_hook(&mut root, "Stop", "tsg --agent-state done"));
        assert!(!add_hook(&mut root, "Stop", "tsg --agent-state done"));
        assert_eq!(commands_of(&root, "Stop").count(), 1);
    }

    #[test]
    fn removing_takes_only_what_we_added() {
        let mut root = json!({
            "hooks": { "Stop": [ { "hooks": [ { "type": "command", "command": "keep-me" } ] } ] }
        });
        add_hook(&mut root, "Stop", "tsg --agent-state done");
        assert!(remove_hook(&mut root, "Stop", "tsg --agent-state done"));
        let got: Vec<&str> = commands_of(&root, "Stop").collect();
        assert_eq!(got, vec!["keep-me"], "人のフックまで消した");
    }

    #[test]
    fn removing_the_last_one_cleans_up_the_empty_nest() {
        let mut root = json!({});
        add_hook(&mut root, "Stop", "tsg --agent-state done");
        remove_hook(&mut root, "Stop", "tsg --agent-state done");
        assert!(
            root.get("hooks").and_then(|h| h.get("Stop")).is_none(),
            "空の入れ物が残った"
        );
    }

    #[test]
    fn every_event_maps_to_a_state_we_understand() {
        for a in [Agent::Claude, Agent::Codex] {
            for (_, state) in events(a) {
                assert!(
                    tsg_mux::protocol::AgentState::parse(state).is_some(),
                    "{state} を知らない"
                );
            }
        }
    }
}
