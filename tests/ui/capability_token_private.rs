// This test proves that CapabilityToken::new() is inaccessible outside the
// enforcement module. It must fail to compile.

use cherub::enforcement::capability::CapabilityToken;
use cherub::enforcement::tier::Tier;

fn main() {
    // This line must produce a compile error: new() is pub(super), not pub.
    let _token = CapabilityToken::new(Tier::Observe);
}
