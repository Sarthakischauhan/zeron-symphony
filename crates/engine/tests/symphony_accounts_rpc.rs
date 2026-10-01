//! Symphony account lifecycle through RPC, with all credential stores isolated.
use std::sync::Arc;

use serde_json::{Value, json};
use zeron_engine::{AgentAccounts, AgentAccountsConfig, EngineCore, HarnessRegistry};
use zeron_harness::mock::MockHarness;
use zeron_proto::HarnessId;
use zeron_rpc::methods;

#[tokio::test]
async fn symphony_accounts_rpc_lifecycle() {
    let temp = tempfile::tempdir().unwrap();
    let config = AgentAccountsConfig::isolated(temp.path());
    let oauth = config.symphony_home.join("oauth");
    std::fs::create_dir_all(&oauth).unwrap();
    let file = oauth.join("openai.json");
    let first = json!({"access_token":"fake-first", "account_id":"one", "email":"one@example.com", "extra":{"opaque":true}});
    let second =
        json!({"access_token":"fake-second", "account_id":"two", "email":"two@example.com"});
    let anthropic =
        json!({"access_token":"fake-claude", "account_id":"claude", "email":"claude@example.com"});
    std::fs::write(&file, first.to_string()).unwrap();
    std::fs::write(oauth.join("anthropic.json"), anthropic.to_string()).unwrap();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness { script: Vec::new() }));
    let mut core = EngineCore::assemble(
        &temp.path().join("engine"),
        Arc::new(registry),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    // rpc_service clones this field: replace it BEFORE creating the service.
    // Assembly does not perform account detection; no real credential store is read.
    core.agent_accounts = AgentAccounts::new(config.clone());
    let client = zeron_rpc::memory_client(core.rpc_service());
    let snapshot = client
        .call(methods::LIST_AGENT_ACCOUNTS, json!({"forceUsage":false}))
        .await
        .unwrap();
    assert!(snapshot["warnings"].is_array());
    let accounts = snapshot["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 2);
    let first_id = accounts
        .iter()
        .find(|a| a["email"] == "one@example.com")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        accounts
            .iter()
            .all(|a| a["harness"] == "symphony" && a["active"] == true)
    );

    let start = client
        .call(
            methods::START_AGENT_LOGIN,
            json!({"harness":"symphony", "provider":"anthropic"}),
        )
        .await
        .unwrap();
    assert_eq!(start["mode"], "paste-code");
    assert!(start["url"].as_str().unwrap().contains("state="));
    let login_id = start["loginId"].as_str().unwrap();
    let pending = client
        .call(methods::POLL_AGENT_LOGIN, json!({"loginId":login_id}))
        .await
        .unwrap();
    assert_eq!(pending["status"], "pending");
    let error = client
        .call(
            methods::COMPLETE_AGENT_LOGIN,
            json!({"loginId":login_id,"code":"fake-code#wrong-state"}),
        )
        .await
        .expect_err("wrong state must fail before any network request");
    assert!(error.to_string().contains("state mismatch"), "{error}");
    assert_eq!(read_json(&oauth.join("anthropic.json")), anthropic);
    let cancelled = client
        .call(methods::CANCEL_AGENT_LOGIN, json!({"loginId":login_id}))
        .await
        .unwrap();
    assert_eq!(cancelled["ok"], true);
    assert!(
        client
            .call(methods::POLL_AGENT_LOGIN, json!({"loginId":login_id}))
            .await
            .is_err()
    );
    assert!(
        client
            .call(
                methods::COMPLETE_AGENT_LOGIN,
                json!({"loginId":login_id,"code":"fake-code#wrong-state"})
            )
            .await
            .is_err()
    );

    // Simulate a different already-minted login without contacting a provider.
    std::fs::write(&file, second.to_string()).unwrap();
    let snapshot = client
        .call(methods::LIST_AGENT_ACCOUNTS, json!({"forceUsage":false}))
        .await
        .unwrap();
    assert_eq!(snapshot["accounts"].as_array().unwrap().len(), 3);
    let second_id = snapshot["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["email"] == "two@example.com")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let activated = client
        .call(
            methods::ACTIVATE_AGENT_ACCOUNT,
            json!({"harness":"symphony","accountId":first_id}),
        )
        .await
        .unwrap();
    assert!(
        activated["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == first_id && a["active"] == true)
    );
    assert_eq!(read_json(&file), first);
    assert_eq!(read_json(&oauth.join("anthropic.json")), anthropic);
    let forgotten = client
        .call(
            methods::FORGET_AGENT_ACCOUNT,
            json!({"harness":"symphony","accountId":first_id}),
        )
        .await
        .unwrap();
    assert!(
        !file.exists(),
        "forgetting the active login removes its live tokens"
    );
    assert!(
        forgotten["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["id"] != first_id)
    );
    assert!(
        forgotten["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == second_id && a["active"] == false)
    );
    assert_eq!(read_json(&oauth.join("anthropic.json")), anthropic);
    let forgotten = client
        .call(
            methods::FORGET_AGENT_ACCOUNT,
            json!({"harness":"symphony","accountId":second_id}),
        )
        .await
        .unwrap();
    assert_eq!(forgotten["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(read_json(&oauth.join("anthropic.json")), anthropic);
    core.shutdown().await;
}

fn read_json(path: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}
