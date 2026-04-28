//! ship — stub for EPIC #152. Brief 4 wires the validator pipeline; brief 6 makes this the only path to publication.

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "ship", about = "EPIC #152 stub — brief 4 wires the pipeline")]
struct Args {
    #[arg(long)]
    commit_message: Option<String>,
}

fn stub_output() -> String {
    serde_json::json!({"ok": true, "stub": true}).to_string()
}

fn main() {
    let _args = Args::parse();
    println!("{}", stub_output());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_returns_ok_json() {
        let line = stub_output();
        let v: serde_json::Value = serde_json::from_str(&line).expect("stub output is valid JSON");
        assert_eq!(v["ok"], serde_json::Value::Bool(true));
        assert_eq!(v["stub"], serde_json::Value::Bool(true));
        assert!(!line.contains('\n'), "stub must be a single line");
    }
}
