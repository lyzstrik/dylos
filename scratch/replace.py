with open("crates/dylos-core/tests/net_setup_integration.rs", "r") as f:
    text = f.read()

import re
replacement = """fn check_no_other_globals(addrs: &Value, ifname: &str, allowed_globals: &[&str]) -> Result<(), Box<dyn Error>> {
    for iface in addrs.as_array().ok_or("not array")? {
        if iface["ifname"].as_str().ok_or("no ifname")? == ifname
            && let Some(addr_info) = iface["addr_info"].as_array()
        {
            let mut has_link_local = false;
            for addr in addr_info {
                let scope = addr["scope"].as_str().unwrap_or("");
                let local = addr["local"].as_str().ok_or("no local")?;
                if scope == "link" && local.starts_with("fe80:") {
                    has_link_local = true;
                } else if scope == "global" {
                    assert!(
                        allowed_globals.contains(&local),
                        "unexpected global address: {local}"
                    );
                }
            }
            assert!(
                has_link_local,
                "missing link-local fe80:: address on {ifname}"
            );
        }
    }
    Ok(())
}"""

# regex replace
text = re.sub(r'fn check_no_other_globals.*?Ok\(\(\)\)\n\}', replacement, text, flags=re.DOTALL)
with open("crates/dylos-core/tests/net_setup_integration.rs", "w") as f:
    f.write(text)
