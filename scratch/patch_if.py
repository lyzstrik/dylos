with open("crates/dylos-core/tests/net_setup_integration.rs", "r") as f:
    text = f.read()
import re
text = re.sub(
    r'if iface\["ifname"\].as_str\(\).ok_or\("no ifname"\)\? == ifname \{\s*if let Some\(addr_info\) = iface\["addr_info"\].as_array\(\) \{',
    r'if iface["ifname"].as_str().ok_or("no ifname")? == ifname && let Some(addr_info) = iface["addr_info"].as_array() {',
    text
)
# remove the closing brace for the outer if
text = text.replace(
"""                assert!(has_link_local, "missing link-local fe80:: address on {ifname}");
            }
        }
    }
    Ok(())""",
"""                assert!(has_link_local, "missing link-local fe80:: address on {ifname}");
        }
    }
    Ok(())""")
with open("crates/dylos-core/tests/net_setup_integration.rs", "w") as f:
    f.write(text)
