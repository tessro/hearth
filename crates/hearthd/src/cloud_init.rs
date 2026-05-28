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
    groups: [adm, sudo, docker]
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL{key_block}
package_update: true
packages:
  - ca-certificates
  - curl
  - gnupg
  - docker.io
  - docker-compose-plugin
runcmd:
  - [ systemctl, enable, --now, docker ]
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
