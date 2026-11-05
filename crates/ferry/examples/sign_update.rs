//! 给更新资产 / manifest 签名（CI release.yml manifest job 与本地手动补签共用）。
//!
//! 用法：
//! ```shell
//! # 本地（默认从 .sign/ 取私钥和口令）
//! cargo run --release --example sign_update -- <待签名文件>
//! # CI（私钥从 GitHub Secrets 落盘、口令走环境注入）
//! cargo run --release --example sign_update -- --key <私钥> --password-file <口令文件> <文件>
//! ```
//! 签名（minisign .sig 全文）打到 stdout，调用方捕获后嵌入 update-manifest.json。
//!
//! 本工具只进 dev 环境（minisign 是 dev-dependency），客户端运行时用零依赖的
//! minisign-verify——签名能力与验证能力分离，release binary 不含私钥处理代码。

use std::io::Read;

const DEFAULT_KEY: &str = ".sign/minisign.key";
const DEFAULT_PWD_FILE: &str = ".sign/minisign.pwd";

fn main() {
    let mut key_path: Option<String> = None;
    let mut password: Option<String> = None;
    let mut password_file: Option<String> = None;
    let mut target: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            a if a.starts_with("--key=") => key_path = Some(a.trim_start_matches("--key=").into()),
            "--key" => key_path = args.next(),
            a if a.starts_with("--password=") => {
                password = Some(a.trim_start_matches("--password=").into())
            }
            "--password" => password = args.next(),
            a if a.starts_with("--password-file=") => {
                password_file = Some(a.trim_start_matches("--password-file=").into())
            }
            "--password-file" => password_file = args.next(),
            a if !a.starts_with("--") && target.is_none() => target = Some(a.into()),
            other => {
                eprintln!("未知参数：{}", other);
                std::process::exit(2);
            }
        }
    }
    let key_path = key_path.unwrap_or_else(|| DEFAULT_KEY.to_string());
    let target = target.unwrap_or_else(|| {
        eprintln!("缺少待签名文件参数");
        std::process::exit(2);
    });

    // 口令必须解析出非 None 值：minisign 的 into_secret_key(None) 会转交互式
    // stdin 询问（CI 卡死），解析不到就报错而不是交给库去问。
    let password = if let Some(p) = password {
        p
    } else {
        let pwd_file = password_file.unwrap_or_else(|| DEFAULT_PWD_FILE.to_string());
        match std::fs::read_to_string(&pwd_file) {
            Ok(p) => p.trim().to_string(),
            Err(e) => {
                eprintln!("缺少口令：--password 未给，且读取 {} 失败：{}", pwd_file, e);
                std::process::exit(2);
            }
        }
    };

    let key_str = std::fs::read_to_string(&key_path)
        .unwrap_or_else(|e| panic!("读取私钥 {} 失败：{}", key_path, e));
    let sk_box = minisign::SecretKeyBox::from_string(&key_str).expect("私钥文件格式非法");
    let sk = sk_box.into_secret_key(Some(password)).expect("私钥口令错误或私钥损坏");

    let mut data = Vec::new();
    std::fs::File::open(&target)
        .and_then(|mut f| f.read_to_end(&mut data))
        .unwrap_or_else(|e| panic!("读取 {} 失败：{}", target, e));

    // trusted comment 带文件名：验签通过后可追溯「这个签名属于哪个文件」，
    // 防止把 A 资产的签名挪用到 B 资产上（minisign 的 trusted comment 由
    // 签名端写入、验签端强制校验）。首参 pk 为 None（签名不绑定公钥注释）。
    let comment = format!(
        "ferry update asset: {}",
        std::path::Path::new(&target).file_name().unwrap().to_string_lossy()
    );
    let sig = minisign::sign(None, &sk, &data[..], Some(&comment), None).expect("签名失败");
    // SignatureBox 实现的是 From<SignatureBox> for String（.sig 全文），非 Display
    println!("{}", String::from(sig));
}
