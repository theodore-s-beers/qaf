#[path = "../src/sandbox.rs"]
mod sandbox;

use sandbox::Sandbox;
use serde_json::{Value, json};
use std::{path::Path, process::Output, time::Duration};

fn setup() -> (tempfile::TempDir, Sandbox) {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project with spaces");
    let credentials = project.join("credentials");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&credentials).unwrap();
    std::fs::write(credentials.join("secret"), "private").unwrap();
    for name in [".git", ".jj"] {
        std::fs::create_dir(project.join(name)).unwrap();
        std::fs::write(project.join(name).join("config"), "original").unwrap();
    }
    let sandbox =
        Sandbox::new(&project, &credentials, Path::new(env!("CARGO_BIN_EXE_qaf"))).unwrap();
    (root, sandbox)
}

async fn output(mut command: tokio::process::Command) -> Output {
    tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .expect("sandbox process hung")
        .expect("sandbox could not launch")
}

async fn file(sandbox: &Sandbox, name: &str, arguments: Value, unrestricted: bool) -> Value {
    let request = serde_json::to_vec(&json!({"name": name, "arguments": arguments})).unwrap();
    let (command, _request_file) = sandbox.file_command(&request, unrestricted).unwrap();
    let result = output(command).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

#[tokio::test]
async fn file_worker_enforces_paths_and_symlinks() {
    let (root, sandbox) = setup();
    sandbox.check().await.unwrap();
    let outside = root.path().join("outside");
    std::fs::write(&outside, "unchanged").unwrap();
    std::os::unix::fs::symlink(&outside, sandbox.project().join("escape")).unwrap();

    assert_eq!(
        file(
            &sandbox,
            "write_file",
            json!({"path":"inside","content":"hello"}),
            false
        )
        .await["ok"],
        true
    );
    assert_eq!(
        file(&sandbox, "read_file", json!({"path":"inside"}), false).await["content"],
        "hello"
    );
    assert_eq!(
        file(
            &sandbox,
            "edit_file",
            json!({"path":"inside","old":"hello","new":"world"}),
            false
        )
        .await["ok"],
        true
    );
    for path in [
        "../outside",
        "escape",
        ".git/config",
        ".jj/config",
        "credentials/secret",
    ] {
        assert!(
            file(
                &sandbox,
                "write_file",
                json!({"path":path,"content":"bad"}),
                false
            )
            .await
            .get("error")
            .is_some(),
            "{path}"
        );
    }
    assert!(
        file(
            &sandbox,
            "edit_file",
            json!({"path":"escape","old":"unchanged","new":"bad"}),
            false
        )
        .await
        .get("error")
        .is_some()
    );
    assert!(
        file(
            &sandbox,
            "read_file",
            json!({"path":"credentials/secret"}),
            false
        )
        .await
        .get("error")
        .is_some()
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged");
    assert_eq!(
        std::fs::read_to_string(sandbox.project().join(".git/config")).unwrap(),
        "original"
    );
    // This tests the launcher capability; agent tests separately require approval.
    assert_eq!(
        file(
            &sandbox,
            "write_file",
            json!({"path":outside,"content":"approved"}),
            true
        )
        .await["ok"],
        true
    );
}

#[tokio::test]
async fn shell_and_children_share_write_restrictions() {
    let (root, sandbox) = setup();
    let mut command = sandbox.shell_command(
        r#"printf ok > inside; printf scratch > "$TMPDIR/item"; sh -c 'printf bad > "$1"' sh "$1""#, false).unwrap();
    command.arg("test").arg(root.path().join("outside"));
    let result = output(command).await;
    assert!(!result.status.success());
    assert_eq!(
        std::fs::read_to_string(sandbox.project().join("inside")).unwrap(),
        "ok"
    );
    assert!(!root.path().join("outside").exists());
    let result = output(
        sandbox
            .shell_command(r#"test -f "$TMPDIR/item""#, false)
            .unwrap(),
    )
    .await;
    assert!(result.status.success());
}

#[tokio::test]
async fn network_and_host_unix_sockets_require_unrestricted_execution() {
    let (_root, sandbox) = setup();
    sandbox.check().await.unwrap();
    let probe = sandbox.project().join("probe");
    let compile = std::process::Command::new("rustc")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sandbox-probe.rs"))
        .arg("-o")
        .arg(&probe)
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    // Deliberately put the socket in the readable project, not an overmounted /tmp.
    let socket = sandbox.project().join("host.sock");
    let unix = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    for (kind, address) in [
        ("tcp", tcp.local_addr().unwrap().to_string()),
        ("unix", socket.to_str().unwrap().to_owned()),
    ] {
        for unrestricted in [true, false] {
            let mut command = sandbox
                .shell_command(r#"exec "$1" "$2" "$3""#, unrestricted)
                .unwrap();
            command.arg("test").arg(&probe).arg(kind).arg(&address);
            let result = output(command).await;
            assert_eq!(
                result.status.success(),
                unrestricted,
                "{kind}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
    drop(unix);
}

#[tokio::test]
async fn sandbox_launcher_is_independent_of_path() {
    let (_root, sandbox) = setup();
    sandbox.check().await.unwrap();
    // PATH supplied by a project cannot replace our platform launcher.
    let mut command = sandbox.shell_command("printf ok", false).unwrap();
    command.env("PATH", "/nonexistent");
    assert_eq!(output(command).await.stdout, b"ok");
}
