//! Both public Auth paths are a compatibility contract.

use marion_harness::Auth as HarnessAuth;
use marion_harness::adapter::Auth as AdapterAuth;

#[test]
fn public_auth_imports_share_the_wire_contract() {
    for (harness, adapter, wire) in [
        (HarnessAuth::Canned, AdapterAuth::Canned, "canned"),
        (HarnessAuth::Inherited, AdapterAuth::Inherited, "inherited"),
    ] {
        let _: HarnessAuth = adapter;
        assert_eq!(harness.as_wire(), wire);
        assert_eq!(adapter.as_wire(), wire);
        assert_eq!(HarnessAuth::from_wire(wire), Some(harness));
        assert_eq!(AdapterAuth::from_wire(wire), Some(adapter));
    }
}
