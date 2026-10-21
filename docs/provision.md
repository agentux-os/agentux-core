# Provisioning machines

`aux provision` sets an AgentUX machine up from one file, with no one at it: the profile, the user whose agents run there, the AgentUX Command server to join, the node policy and the agent preferences. It is meant for fleets of virtual machines: each VM gets its own file (with its own single-use enrollment token) and comes up already in its role, connected to its Command server.

```yaml
version: 1
profile: node                 # workstation, node, command, executive, complete, custom
hostname: build-01            # optional
user: dev                     # whose agentuxd and gateway run here (must exist)
command:                      # optional: join this Command server
  url: https://command.acme.org
  enrollmentToken: auxe1_cmd-…_…   # single use, issued by that server
  nodeName: build-01          # optional
policy:                       # optional: /etc/agentux/node-policy.yaml
  commands:
    run.start: allow
preferences:                  # optional: the user's ~/.config/agentux/agents.yaml
  order: [opencode, claude-code]
  idleMinutes: 15
```

JSON works too. The file is checked whole before anything changes: unknown keys, profiles, policy methods or a token that is not an enrollment token are refused.

## What it does

As root, in order (each step can run again safely):

1. Sets the host name (`hostnamectl`).
2. Records the profile (`agentux profile seed`); `agentux-profile-apply.service` applies it.
3. Writes the node policy.
4. Lets the user's services run without a login (`loginctl enable-linger`), so a headless node's agentuxd and gateway run.
5. Writes the user's agent preferences, owned by the user.
6. Joins Command as the user (`aux node enroll`, the token in the environment, never in a command line) and starts `agentuxd` and `agentux-gateway`. The node pins that server's instance id (see [gateway.md](gateway.md)).

Then it records `/var/lib/agentux/provisioned.json`; a machine is provisioned once.

```sh
sudo aux provision apply build-01.yaml --dry-run   # check only
sudo aux provision apply build-01.yaml
sudo aux provision auto                            # what the first-boot service runs
```

## Where `auto` finds the file

On first boot, `agentux-provision.service` (agentux-os) runs `aux provision auto`, which takes the first of:

| Source | How to provide it |
|---|---|
| Kernel command line `agentux.provision=/path` | A file the installer put there (kickstart `%post`) |
| QEMU fw_cfg `opt/org.agentux/provision` | libvirt / QEMU / Proxmox, no disk needed (below) |
| A filesystem labelled `AGENTUX` with `agentux-provision.yaml` | Any hypervisor: attach a small ISO or disk |
| `/boot/agentux-provision.yaml`, `/etc/agentux/provision.yaml` | Disk images prepared before first boot |

A file in `/boot` or `/etc` is removed after it is applied: its token is spent.

## Examples

QEMU and libvirt (`virt-install`):

```sh
virt-install … --qemu-commandline='-fw_cfg name=opt/org.agentux/provision,file=/srv/agentux/build-01.yaml'
```

Proxmox (`/etc/pve/qemu-server/<vmid>.conf`):

```
args: -fw_cfg name=opt/org.agentux/provision,file=/srv/agentux/build-01.yaml
```

Any hypervisor, with an ISO:

```sh
genisoimage -V AGENTUX -r -J -o build-01-provision.iso agentux-provision.yaml
```

Many VMs: issue one token per VM at the Command server (`POST /v1/enrollment-tokens` with `count`), write one file per token, and give each VM its file.
