use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{exit, Command};
use std::{env, fs};

const SSH_CONFIG: &str = r#"Host bastion
  HostName bastion.example.com
  User admin
  Port 2222

Host prod-web-01
  HostName web01.prod.example.com
  User deploy

Host prod-web-02
  HostName web02.prod.example.com
  User deploy

Host prod-db-01
  HostName db01.prod.example.com
  User postgres

Host staging-web-01
  HostName web01.staging.example.com
  User deploy

Host staging-db-01
  HostName db01.staging.example.com
  User postgres

Host "Home NAS"
  HostName 192.168.1.42
  User nathanael

Host "Raspberry Pi"
  HostName 192.168.1.10
  User pi

Host "Proxy jump example"
  HostName internal.example.com
  User someone
  ProxyJump bastion
"#;

const SSH_SHIM: &str = r#"#!/bin/bash
sleep 0.4
echo "Welcome to Ubuntu 24.04.1 LTS (GNU/Linux 6.8.0-51-generic x86_64)"
echo
echo "Last login: Fri Aug 29 10:12:03 2026 from 203.0.113.7"
printf 'deploy@%s:~$ ' "$1"
sleep 8
"#;

fn main() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));

    if Command::new("vhs").arg("--version").output().is_err() {
        eprintln!("vhs not found. Install it first: brew install vhs");
        exit(1);
    }

    run(Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(repo));

    let work = repo.join("target/demo");
    let home = work.join("home");
    fs::create_dir_all(home.join(".ssh")).unwrap();
    fs::write(home.join(".ssh/config"), SSH_CONFIG).unwrap();

    let shim = work.join("ssh");
    fs::write(&shim, SSH_SHIM).unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();

    let tape = format!(
        r#"Output demo.gif

Set FontSize 20
Set Width 1200
Set Height 680
Set Padding 20
Set WindowBar Colorful
Set Shell bash

Hide
Type `export HOME="{home}" PATH="{work}:{release}:$PATH" && clear`
Enter
Show

Sleep 500ms
Type@120ms "sshs"
Sleep 500ms
Enter
Sleep 2s

# Navigate the host list
Down@400ms 4
Sleep 800ms
Up@400ms 2
Sleep 1s

# Search: every whitespace-separated token must match
Type@180ms "web"
Sleep 1.2s
Type@180ms " prod"
Sleep 1.5s

# Pick the second result
Down
Sleep 1s

# Connect (runs the fake ssh script)
Enter
Sleep 4s
"#,
        home = home.display(),
        work = work.display(),
        release = repo.join("target/release").display(),
    );
    fs::write(work.join("demo.tape"), tape).unwrap();

    run(Command::new("vhs").arg("demo.tape").current_dir(&work));

    let out = repo.join(".github/demo");
    fs::create_dir_all(&out).unwrap();
    fs::rename(work.join("demo.gif"), out.join("demo.gif")).unwrap();
    println!("Wrote .github/demo/demo.gif");
}

fn run(cmd: &mut Command) {
    let status = cmd.status().expect("failed to start command");
    if !status.success() {
        eprintln!("command failed: {cmd:?}");
        exit(1);
    }
}
