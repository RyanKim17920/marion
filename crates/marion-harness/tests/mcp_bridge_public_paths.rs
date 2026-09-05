use marion_core::contract::AgentId;
use marion_harness::AGENT_ID_ENV as FLAT_AGENT_ID_ENV;
use marion_harness::claude_code::AGENT_ID_ENV as COMPAT_AGENT_ID_ENV;
use marion_harness::mcp_bridge::{AGENT_ID_ENV, BridgeEnv};

#[test]
fn neutral_and_compatibility_paths_name_one_bridge_contract() {
    assert_eq!(AGENT_ID_ENV, "MARION_AGENT_ID");
    assert_eq!(FLAT_AGENT_ID_ENV, AGENT_ID_ENV);
    assert_eq!(COMPAT_AGENT_ID_ENV, AGENT_ID_ENV);
    let bridge = BridgeEnv {
        bridge: "/bin/marion-supervisor".into(),
        args: vec!["mcp".into()],
        repo: "/repo".into(),
        state: "/state".into(),
        base_url: None,
        auth: marion_harness::Auth::Inherited,
        agent_id: AgentId("019f-root".into()),
        agent_type: "claude".into(),
        depth: 0,
        node_token: None,
        ready_file: None,
    };
    assert!(
        bridge
            .pairs()
            .contains(&(AGENT_ID_ENV.to_string(), "019f-root".to_string()))
    );
}
