//! 生成更新签名用的 minisign 密钥对（一次性开发者动作）。
//!
//! 用法：
//! ```shell
//! cargo run --example keygen_update -- --password "你的私钥口令"
//! ```
//!
//! 产物（`.sign/` 已 gitignore，绝不提交；CI 走 GitHub Secrets）：
//! - `.sign/minisign.key`   私钥（口令加密）
//! - `.sign/minisign.pwd`   口令明文（本地 sign_update 默认从这里读）
//! - `assets/update-pubkey.txt` 公钥（提交进仓库，客户端 include_str! 内嵌验签）
//!
//! CI 配置（仓库 Settings → Secrets and variables → Actions）：
//! - `UPDATE_MINISIGN_KEY`      ← `.sign/minisign.key` 文件的完整内容
//! - `UPDATE_MINISIGN_PASSWORD` ← 私钥口令

use minisign::KeyPair;

fn main() {
    let mut password: Option<String> = None;
    let mut force = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--force" => force = true,
            a if a.starts_with("--password=") => {
                password = Some(a.trim_start_matches("--password=").to_string())
            }
            _ => {
                eprintln!("未知参数：{}（只支持 --password=<口令> 和 --force）", arg);
                std::process::exit(2);
            }
        }
    }
    // 口令必填：minisign 无口令时转交互式 stdin 询问，CI / 后台环境直接卡死。
    let password = match password {
        Some(p) => p,
        None => {
            eprintln!("用法：cargo run --example keygen_update -- --password=<私钥口令> [--force]");
            std::process::exit(2);
        }
    };

    let dir = std::path::Path::new(".sign");
    let sk_path = dir.join("minisign.key");
    let pwd_path = dir.join("minisign.pwd");
    let repo_pk = std::path::Path::new("assets/update-pubkey.txt");

    if sk_path.exists() && !force {
        eprintln!(
            "已存在 {}（用 --force 覆盖；轮换密钥请阅读本文件顶部注释）",
            sk_path.display()
        );
        std::process::exit(1);
    }
    std::fs::create_dir_all(dir).expect("创建 .sign 目录失败");
    std::fs::create_dir_all(repo_pk.parent().unwrap()).expect("创建 assets 目录失败");

    let KeyPair { pk, sk } =
        KeyPair::generate_encrypted_keypair(Some(password.clone())).expect("生成密钥对失败");
    let sk_box = sk.to_box(None).expect("序列化私钥失败");
    let pk_box = pk.to_box().expect("序列化公钥失败");
    std::fs::write(&sk_path, sk_box.to_string()).expect("写私钥失败");
    std::fs::write(&pwd_path, &password).expect("写口令文件失败");
    std::fs::write(repo_pk, pk_box.to_string()).expect("写 assets/update-pubkey.txt 失败");

    println!("私钥：{}（.sign/ 已 gitignore，勿提交、勿外传）", sk_path.display());
    println!("口令：{}", pwd_path.display());
    println!("已写入 {}（提交进仓库，客户端编译期内嵌）", repo_pk.display());
    println!();
    println!("配置 CI（仓库 Settings → Secrets and variables → Actions）：");
    println!("  UPDATE_MINISIGN_KEY      ← .sign/minisign.key 的完整内容");
    println!("  UPDATE_MINISIGN_PASSWORD ← 私钥口令");
}
