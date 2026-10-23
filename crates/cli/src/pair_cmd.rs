//! `pair` 子命令：发起配对（默认）或加入配对（`--qr` / `--code`）。
//!
//! 双方必须比对 6 位 SAS 后才会保存 PairRecord；`--yes` 用于脚本/测试自动确认。

use std::io::{self, Write};

use anyhow::{bail, Result};
use derive_client::{pair_initiate, pair_respond, PairOutcome, PairUi, RuntimeConfig};
use derive_core::identity::PublicIdentity;

struct CliPairUi {
    yes: bool,
}

impl PairUi for CliPairUi {
    fn on_qr(&self, qr: &str) {
        println!();
        println!("二维码串（可用另一台设备 `pair --qr` 粘贴/扫码）:");
        println!("{qr}");
        println!();
    }

    fn on_code(&self, code: &str) {
        println!("配对码: {code}  （在另一台设备上执行 `derive-sync pair --code {code}`）");
    }

    fn on_peer(&self, peer: &PublicIdentity) {
        println!("对端设备: {} ({})", peer.name, peer.device_id);
    }

    fn on_sas(&self, sas: &str) {
        println!("SAS 校验码: {sas}");
    }

    fn confirm_sas(&self, sas: &str) -> bool {
        if self.yes {
            println!("已通过 --yes 自动确认 SAS。");
            return true;
        }
        print!("请确认另一台设备显示的 SAS 也是 `{sas}` [y/N]: ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => {
                eprintln!("未读取到确认输入（非交互环境请加 --yes）。");
                false
            }
            Ok(_) => {
                let answer = line.trim().to_ascii_lowercase();
                answer == "y" || answer == "yes"
            }
            Err(_) => false,
        }
    }
}

fn print_outcome(outcome: &PairOutcome) {
    println!();
    println!("配对成功！");
    println!(
        "  对端: {} ({})",
        outcome.record.peer.name, outcome.record.peer.device_id
    );
    println!("  配对指纹: {}", outcome.record.pairing_fingerprint);
    println!("  已保存到 peers.json");
}

pub async fn run(
    config: &RuntimeConfig,
    qr: Option<String>,
    code: Option<String>,
    yes: bool,
) -> Result<()> {
    if qr.is_some() && code.is_some() {
        bail!("--qr 与 --code 只能二选一");
    }
    let ui = CliPairUi { yes };
    let outcome = if let Some(qr_string) = qr {
        println!("正在加入配对（扫码模式）...");
        pair_respond(config, Some(&qr_string), None, &ui).await?
    } else if let Some(code) = code {
        println!("正在加入配对（配对码模式）...");
        pair_respond(config, None, Some(&code), &ui).await?
    } else {
        println!("正在发起配对（等待另一台设备加入，最长 120 秒）...");
        pair_initiate(config, &ui).await?
    };
    print_outcome(&outcome);
    Ok(())
}
