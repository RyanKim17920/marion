use std::ffi::OsString;

use marion_harness::AGENT_ID_ENV as FLAT_AGENT_ID_ENV;
use marion_harness::claude_code::AGENT_ID_ENV as COMPAT_AGENT_ID_ENV;
use marion_harness::mcp_bridge::{AGENT_ID_ENV, MarionMcpBridge};

#[test]
fn neutral_and_compatibility_paths_name_one_bridge_contract() {
    assert_eq!(AGENT_ID_ENV, "MARION_AGENT_ID");
    assert_eq!(FLAT_AGENT_ID_ENV, AGENT_ID_ENV);
    assert_eq!(COMPAT_AGENT_ID_ENV, AGENT_ID_ENV);
    let bridge = MarionMcpBridge {
        program: OsString::from("/bin/marion-supervisor"),
        args: vec![OsString::from("mcp")],
        env: vec![(OsString::from(AGENT_ID_ENV), OsString::from("019f-root"))],
    };
    assert_eq!(bridge.args, [OsString::from("mcp")]);
}
