use crate::registry::Service;

pub fn user_data(service: &Service) -> String {
    let keys = service
        .cloud_init
        .ssh_keys
        .iter()
        .map(|key| format!("      - {key}"))
        .collect::<Vec<_>>()
        .join("\n");
    let key_block = if keys.is_empty() {
        String::new()
    } else {
        format!("\n    ssh_authorized_keys:\n{keys}")
    };
    format!(
        r#"#cloud-config
hostname: {hostname}
manage_etc_hosts: true
users:
  - default
  - name: {user}
    groups: [adm, sudo]
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL{key_block}
"#,
        hostname = service.cloud_init.hostname,
        user = service.cloud_init.user,
        key_block = key_block
    )
}

pub fn meta_data(service: &Service) -> String {
    format!(
        "instance-id: hearth-{}\nlocal-hostname: {}\n",
        service.name, service.cloud_init.hostname
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{CloudInit, Provision, RestartPolicy, Service};

    fn service_with_keys(keys: Vec<&str>) -> Service {
        Service {
            name: "web".into(),
            enabled: false,
            image: "debian".into(),
            cpu: 2,
            memory_mib: 2048,
            disk_gib: 20,
            vsock_cid: 100,
            mac: "52:54:00:00:00:01".into(),
            is_agent_in_charge: false,
            disk: None,
            publish: Vec::new(),
            cloud_init: CloudInit {
                hostname: "web".into(),
                ssh_keys: keys.into_iter().map(str::to_owned).collect(),
                user: "agent".into(),
            },
            provision: Provision::default(),
            restart: RestartPolicy::default(),
        }
    }

    #[test]
    fn user_data_renders_minimal_cloud_config_with_keys() {
        let svc = service_with_keys(vec!["ssh-ed25519 AAAA one", "ssh-ed25519 AAAA two"]);
        let expected = "\
#cloud-config
hostname: web
manage_etc_hosts: true
users:
  - default
  - name: agent
    groups: [adm, sudo]
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
    ssh_authorized_keys:
      - ssh-ed25519 AAAA one
      - ssh-ed25519 AAAA two
";
        assert_eq!(user_data(&svc), expected);
    }

    #[test]
    fn user_data_omits_authorized_keys_block_when_empty() {
        let svc = service_with_keys(vec![]);
        let rendered = user_data(&svc);
        assert!(!rendered.contains("ssh_authorized_keys"));
        assert!(rendered.contains("sudo: ALL=(ALL) NOPASSWD:ALL\n"));
    }

    #[test]
    fn user_data_no_longer_installs_docker() {
        let svc = service_with_keys(vec![]);
        let rendered = user_data(&svc);
        assert!(!rendered.contains("docker"));
        assert!(!rendered.contains("packages:"));
        assert!(!rendered.contains("runcmd:"));
    }

    #[test]
    fn meta_data_uses_hearth_instance_id() {
        let svc = service_with_keys(vec![]);
        assert_eq!(
            meta_data(&svc),
            "instance-id: hearth-web\nlocal-hostname: web\n"
        );
    }
}
