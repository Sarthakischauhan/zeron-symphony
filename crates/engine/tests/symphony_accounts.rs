//! File-backed Symphony OAuth account lifecycle; never touches real credentials.
use serde_json::{Value, json};
use std::{fs, path::Path};
use zeron_engine::{AgentAccounts, AgentAccountsConfig};
use zeron_proto::{AgentAccountsSnapshot, HarnessId};

fn token(identity: &str) -> Value {
    // Matches core_ai.oauth.types.OAuthToken.to_json(), including non-JWT tokens.
    json!({
        "access_token": format!("secret-access-{identity}"),
        "refresh_token": format!("secret-refresh-{identity}"),
        "id_token": format!("secret-id-{identity}"),
        "token_type": "Bearer", "expires_at": 4102444800.0,
        "account_id": identity, "scope": "openid profile offline_access"
    })
}

fn write_token(root: &Path, provider: &str, value: &Value) {
    let dir = root.join("symphony/oauth");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{provider}.json")),
        serde_json::to_vec(value).unwrap(),
    )
    .unwrap();
}

fn assert_public(snapshot: &AgentAccountsSnapshot) {
    let wire = serde_json::to_string(snapshot).unwrap();
    for secret in [
        "secret-access-",
        "secret-refresh-",
        "secret-id-",
        "access_token",
        "refresh_token",
        "id_token",
    ] {
        assert!(
            !wire.contains(secret),
            "public account snapshot leaked {secret}"
        );
    }
}

fn assert_private_tree(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::metadata(path).unwrap();
        assert_eq!(
            metadata.permissions().mode() & 0o077,
            0,
            "{} is not private",
            path.display()
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                assert_private_tree(&entry.unwrap().path());
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[tokio::test]
async fn detects_each_oauth_provider_without_exposing_secrets() {
    let tmp = tempfile::tempdir().unwrap();
    let config = AgentAccountsConfig::isolated(tmp.path());
    assert_eq!(config.symphony_home, tmp.path().join("symphony"));
    for provider in ["openai", "anthropic", "grok"] {
        write_token(tmp.path(), provider, &token(provider));
    }
    let accounts = AgentAccounts::new(config);
    let snapshot = accounts.list(false).await.unwrap();
    let slots: Vec<_> = snapshot
        .accounts
        .iter()
        .filter(|a| a.harness == HarnessId::Symphony)
        .collect();
    assert_eq!(slots.len(), 3);
    assert!(slots.iter().all(|a| a.active && a.switchable));
    assert_public(&snapshot);
    assert_private_tree(&tmp.path().join("data/agent-accounts"));
}

#[tokio::test]
async fn multiple_saved_slots_activate_and_forget_only_the_selected_provider() {
    let tmp = tempfile::tempdir().unwrap();
    let config = AgentAccountsConfig::isolated(tmp.path());
    let accounts = AgentAccounts::new(config);
    let first = token("openai-first");
    let second = token("openai-second");
    write_token(tmp.path(), "openai", &first);
    let snapshot = accounts.list(false).await.unwrap();
    let first_id = snapshot
        .accounts
        .iter()
        .find(|a| a.harness == HarnessId::Symphony)
        .unwrap()
        .id
        .clone();
    assert_public(&snapshot);

    for provider in ["anthropic", "grok"] {
        write_token(tmp.path(), provider, &token(provider));
    }
    let oauth = tmp.path().join("symphony/oauth");
    let other_files: Vec<_> = ["anthropic", "grok"]
        .iter()
        .map(|p| {
            let path = oauth.join(format!("{p}.json"));
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    write_token(tmp.path(), "openai", &second);
    let snapshot = accounts.list(false).await.unwrap();
    let slots: Vec<_> = snapshot
        .accounts
        .iter()
        .filter(|a| a.harness == HarnessId::Symphony)
        .collect();
    assert_eq!(slots.len(), 4, "two OpenAI slots plus the other providers");
    assert!(!slots.iter().find(|a| a.id == first_id).unwrap().active);
    assert_eq!(slots.iter().filter(|a| a.active).count(), 3);
    assert_public(&snapshot);

    let snapshot = accounts
        .activate(HarnessId::Symphony, &first_id)
        .await
        .unwrap();
    assert!(
        snapshot
            .accounts
            .iter()
            .any(|a| a.id == first_id && a.active)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(oauth.join("openai.json")).unwrap()).unwrap(),
        first
    );
    assert_public(&snapshot);
    assert_private_tree(&oauth.join("openai.json"));
    assert_private_tree(&tmp.path().join("symphony/.env"));
    assert_private_tree(&tmp.path().join("data/agent-accounts"));
    for (path, bytes) in &other_files {
        assert_eq!(
            &fs::read(path).unwrap(),
            bytes,
            "activate changed another provider"
        );
    }

    let snapshot = accounts
        .forget(HarnessId::Symphony, &first_id)
        .await
        .unwrap();
    assert!(!snapshot.accounts.iter().any(|a| a.id == first_id));
    assert!(
        !oauth.join("openai.json").exists(),
        "forget active login signs out that provider"
    );
    assert_eq!(
        snapshot
            .accounts
            .iter()
            .filter(|a| a.harness == HarnessId::Symphony)
            .count(),
        3
    );
    assert_public(&snapshot);
    for (path, bytes) in &other_files {
        assert_eq!(
            &fs::read(path).unwrap(),
            bytes,
            "forget changed another provider"
        );
    }
    let snapshot = accounts.list(false).await.unwrap();
    assert!(
        !snapshot.accounts.iter().any(|a| a.id == first_id),
        "forgotten slot must not reappear"
    );
    assert_public(&snapshot);
}
