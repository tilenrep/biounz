// schedule: 30 3 * * SUN
// Preview only unless apply_updates is explicitly set to true.

let apply_updates = false;
if host_os() != "linux" {
    print("This example only handles Linux package managers. No update attempted.");
    return;
}
if path_exists("/run/ostree-booted") {
    throw "OSTree systems need their own update mechanism";
}

let release_path = if path_exists("/etc/os-release") { "/etc/os-release" } else { "/usr/lib/os-release" };
let release = read_text(release_path);
let id = "";
for line in release.split("\n") {
    if line.starts_with("ID=") {
        id = line.sub_string(3);
        id.trim();
        if (id.starts_with("\"") && id.ends_with("\"")) || (id.starts_with("'") && id.ends_with("'")) {
            id = id.sub_string(1, id.len() - 2);
        }
    }
}

// Exact distribution IDs only; never guess from ID_LIKE or PATH.
let plan = switch id {
    "debian" | "ubuntu" => ["/usr/bin/apt-get", [["update"], ["upgrade", "-y"]]],
    "fedora" => ["/usr/bin/dnf", [["upgrade", "--refresh", "-y"]]],
    "arch" | "cachyos" => ["/usr/bin/pacman", [["-Syu", "--noconfirm"]]],
    _ => throw "Unsupported or missing distribution ID: " + id,
};
let manager = plan[0];
if !path_exists(manager) {
    throw "Package manager not found: " + manager;
}

print("Detected " + id + "; apply updates: " + apply_updates);
for args in plan[1] {
    let preview = manager;
    for arg in args { preview += " " + arg; }
    print(preview);
    if apply_updates {
        let status = run_command(manager, args);
        if status != 0 { throw manager + " exited with status " + status; }
    }
}
