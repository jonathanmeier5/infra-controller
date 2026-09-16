# NiCo Host Ingestion and DHCP Discovery

## About this document

This document explains how NVIDIA Infra Controller (NiCo) finds a bare-metal host
and makes it a managed host. It gives special attention to the DHCP discovery
behavior, because DHCP is the only mechanism that starts the process.

All code references point to `dsx-ai-factory/infra-controller` at commit
`2768e18bf` (`main`, 2026-09-14). The older path `NVIDIA/infra-controller`
redirects to the same repository. Line numbers can move. Check the function name
if a link does not go to the expected code.

Refer to Appendix A for the full sequence diagram.
Refer to Appendix B for a table of all code references.
Refer to Appendix C for the DHCP drop reasons.

---

## 1. Summary

NiCo ingestion has two independent halves:

1. **Registration.** An operator or an upstream system writes a row in the
   `expected_machines` table. This row contains the BMC MAC address, the chassis
   serial number, and the factory-default BMC credentials.
2. **Discovery.** The BMC sends a DHCP request. NiCo gives it an address, finds
   it with Redfish, and adds it to the state machine.

These two halves are not synchronized. You can do them in any order. NiCo runs a
reconciliation loop, so the two halves come together when both are complete.

Four facts control the behavior. Each one is counter-intuitive:

| Fact | Consequence |
|---|---|
| The BMC starts on standby power. | No software event starts ingestion. A person puts in the power cable. |
| NiCo refuses a DHCP packet that no relay forwarded. | You must configure a DHCP relay on every BMC network. |
| DHCP does not examine the `expected_machines` table for permission. | An unregistered BMC still gets an address. |
| Only managed-host creation examines that table. | An unregistered machine is visible, but NiCo does not ingest it. |

---

## 2. Registration

### 2.1 What a registration contains

The `expected_machines` table is an allowlist. It is also the store for the
factory-default BMC credentials.

Each row needs three fields:

- `bmc_mac_address` — the primary key for all matching.
- `chassis_serial_number` — NiCo compares this with the value that Redfish
  reports.
- `defaultBmcUsername` and `defaultBmcPassword` — NiCo uses these for first
  contact. After discovery, NiCo changes the password to the site-wide value.

Refer to
[`ingesting-hosts-rest-api.md`](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/provisioning/ingesting-hosts-rest-api.md#L27)
for the full schema.

### 2.2 How inventory arrives

NiCo does not know which system sends the inventory. It accepts these paths:

| Path | Interface |
|---|---|
| Upstream inventory system (for example, Launch Layer) | `POST /v2/org/{org}/nico/expected-machine[/batch]` |
| Operator, one machine | `nicocli expected-machine create` |
| Operator, whole table | `nico-admin-cli em replace-all` |

The batch endpoint accepts a maximum of 100 machines in one request. All machines
in one request must have the same `siteId`.

### 2.3 The path from REST to Core

The cloud REST API does not write to NiCo Core directly. It uses a Temporal
workflow.

1. The handler validates the request and writes a row in the cloud database.
   Refer to
   [`expectedmachine.go#L89`](https://github.com/dsx-ai-factory/infra-controller/blob/main/rest-api/api/pkg/api/handler/expectedmachine.go#L89).
2. The handler does not keep the BMC credentials. The comment at
   [`#L189`](https://github.com/dsx-ai-factory/infra-controller/blob/main/rest-api/api/pkg/api/handler/expectedmachine.go#L189)
   is explicit: `DefaultBmcUsername and BmcPassword are not stored in DB, only
   passed to workflow`.
3. The handler starts the `CreateExpectedMachine` workflow on the site task
   queue.
4. The workflow calls two activities. Refer to
   [`workflow/expectedmachine.go#L61`](https://github.com/dsx-ai-factory/infra-controller/blob/main/rest-api/site-workflow/pkg/workflow/expectedmachine.go#L61).
   - `CreateExpectedMachineOnSite` writes to Core. A failure fails the workflow.
   - `CreateExpectedMachineOnFlow` writes to Flow. A failure only writes a
     warning.
5. The site activity calls the Core gRPC method `AddExpectedMachine`. Refer to
   [`activity/expectedmachine.go#L283`](https://github.com/dsx-ai-factory/infra-controller/blob/main/rest-api/site-workflow/pkg/activity/expectedmachine.go#L283).
6. The Core handler validates the data and writes the row. Refer to
   [`expected_machine.rs#L72`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-core/src/handlers/expected_machine.rs#L72).

> **Note.** The Flow write is being removed. A Temporal version gate at
> [`workflow/expectedmachine.go#L93`](https://github.com/dsx-ai-factory/infra-controller/blob/main/rest-api/site-workflow/pkg/workflow/expectedmachine.go#L93)
> controls it. New workflows use the Core-only path.

### 2.4 Full-table replacement

`replace_all` deletes the table and writes the new content. Refer to
[`expected_machine.rs#L320`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-core/src/handlers/expected_machine.rs#L320).

The handler validates every entry before it deletes anything. The delete and the
inserts commit together. A database failure keeps the previous inventory.

**A synchronization job that uses `replace_all` must send the complete fleet in
each run.** If it sends a subset, it deletes the remainder.

---

## 3. DHCP discovery

### 3.1 The BMC starts by itself

The BMC is a separate computer on the motherboard. It has its own firmware, its
own network interface, and its own power domain. It runs when the power supply
has AC power. The host power state does not change this.

Therefore:

- No software event starts ingestion.
- The host can be off. The host can have no operating system. The host can have
  no disks.
- The only action that starts the sequence is a person who puts in the power
  cable.

The BMC sends a DHCP DISCOVER packet as soon as its network interface is up.

### 3.2 Why the packet needs a relay

The BMC has no address. It cannot send a normal IP packet. It therefore uses a
layer-2 broadcast:

```
Ethernet dst: ff:ff:ff:ff:ff:ff
Ethernet src: c4:5a:b1:c8:38:0d      <- the only identity the BMC has
IP src:       0.0.0.0
IP dst:       255.255.255.255
UDP:          68 -> 67
DHCP:         DISCOVER
```

A router does not forward a broadcast. If it did, one broadcast would flood the
full datacenter. The broadcast therefore stops at the first layer-3 boundary.

NiCo's DHCP service is a pod in the site controller cluster. A MetalLB
LoadBalancer VIP makes it available. That VIP is many routed hops away from the
BMC. The broadcast cannot reach it.

### 3.3 What the relay agent does

The relay agent is usually the router that owns the BMC VLAN. You configure it
with a helper address. The relay does four things:

1. It listens on UDP port 67 on the VLAN interface.
2. It catches the broadcast.
3. It writes the packet again as a **unicast** IP packet to the DHCP server
   address. The packet is now routable.
4. It writes its own VLAN interface address into the **`giaddr`** field.

Step 4 is the important one. The `giaddr` field does two jobs:

- It tells the server how to send the reply.
- It tells the server **which subnet the client is on**. The server uses this to
  select the correct address pool.

The prerequisites state the rule at
[`bmc-oob-setup.md#L16`](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/getting-started/prerequisites/bmc-oob-setup.md#L16):

> A DHCP relay on every BMC-facing network, pointing to the NICo DHCP service IP
> (`NICo_DHCP_EXTERNAL`)

The word **every** is important. If one rack has no relay, the hosts in that rack
do not appear. There is no error message.

### 3.4 NiCo refuses a packet that has no relay

NiCo does not only accept relayed packets. It **rejects** all other packets.
Refer to
[`packet_handler.rs#L126`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/dhcp-server/src/packet_handler.rs#L126):

```rust
fn is_relayed(&self) -> Result<(), DhcpError> {
    // get gi address
    let giaddress = self.packet.giaddr();
    if giaddress.is_broadcast() || giaddress == Ipv4Addr::new(0, 0, 0, 0) {
        return Err(DhcpError::NonRelayedPacket(giaddress));
    }
    Ok(())
}
```

A `giaddr` of `0.0.0.0` means that no relay touched the packet. NiCo drops it. A
DHCP client on the same segment as the server gets no reply.

The relay is part of the protocol contract. It is not an optimization.

NiCo also reads the DHCP Option 82 `Link Selection` sub-option. Refer to
[`packet_handler.rs#L151`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/dhcp-server/src/packet_handler.rs#L151).
This sub-option gives the client subnet independently of `giaddr`. When it is
present, it controls the pool selection.

### 3.5 `nico-dhcp` is not a DHCP daemon

This surprises many operators.

`nico-dhcp` is a Kea DHCP hooks library (a `cdylib`). The container loads it into
an upstream Kea v4 server. The hooks intercept each DISCOVER and REQUEST packet
and send it to `nico-api` over mTLS gRPC. The RPC is `discover_dhcp`. Refer to
[`ip-and-network-configuration.md#L294`](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/provisioning/ip-and-network-configuration.md#L294).

The `lease4_select` and `lease4_renew` callouts replace the lease that Kea
selected. They replace `yiaddr`, the valid lifetime, and the DHCP options. The
callout can return `SKIP` to cancel Kea's own lease write.

Three consequences for operations:

- **`nico-api` holds the source of truth, not the Kea lease file.** The file
  `kea-leases4.csv` is a copy.
- **There is no DHCP configuration file for reservations.** Reservations come
  from `expected_machines`. Network segments come from `siteConfig`.
- **If `nico-api` is unreachable, the hooks give cached negative replies.** The
  negative cache TTL is 5 minutes. This is a degraded mode, not a fallback pool.
  Expect up to 5 minutes of stale refusals after `nico-api` recovers.

### 3.6 How NiCo selects an address

`nico-api` uses the source MAC address and the relay metadata. Refer to
[`discover.rs#L539`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-core/src/dhcp/discover.rs#L539).

For IPv4, NiCo finds the configured prefix that contains the relay `giaddr`. The
configured gateway does **not** select the segment. For IPv6, NiCo uses an exact
link-address match if one is present, and prefix containment if not.

These interface types use the same `nico-dhcp` instance:

| Interface | Request comes from | Served from |
|---|---|---|
| Host BMC | OOB network, or a shared host network if the host has no managed DPU | The relay-selected `Underlay`, or the shared `HostInband` segment |
| DPU BMC | OOB network | The relay-selected `Underlay` segment |
| DPU OOB (ARM OS) | OOB network | The relay-selected `Underlay` segment |
| Host OS NIC, no managed DPU | Host-facing network | The relay-selected `HostInband` segment |

### 3.7 The four allocation policies

An expected machine can declare a policy for each interface. Refer to
[`expected_machine.rs#L286`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-model/src/expected_machine.rs#L286).

| Policy | Behavior |
|---|---|
| `Dynamic` | A normal DHCP lease. It can expire and change. |
| `Fixed` | NiCo reserves the address that the operator gives in `fixed_ip`. |
| `Retained` | NiCo allocates through DHCP, then makes the address static for the life of the interface record. |
| `Auto` (default) | With an address, this is `Fixed`. With no address, this is `Retained`. |

The validation rules are strict. Refer to
[`expected_machine.rs#L311`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-model/src/expected_machine.rs#L311):

- `Fixed` needs a `fixed_ip`.
- `Dynamic` refuses a `fixed_ip`.
- `Retained` refuses a `fixed_ip`. Use `Fixed` instead.

### 3.8 DHCP does not check the allowlist

This is the most important behavior in this document.

`discover_dhcp` reads the expected machine at
[`discover.rs#L642`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-core/src/dhcp/discover.rs#L642).
The lookup function returns `Ok(None)` when it finds nothing. Refer to
[`discover.rs#L389`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-core/src/dhcp/discover.rs#L389):

```rust
let Some(expected) = expected else {
    return Ok(None);
};
```

`Ok(None)` is not an error. Execution continues.

A `None` result only removes the Fixed preallocation branch at
[`discover.rs#L715`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-core/src/dhcp/discover.rs#L715).
The unregistered BMC gets a normal dynamic address on the relay-selected segment.

A second piece of evidence supports this. The `DropReason` enum at
[`metrics.rs#L100`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/dhcp-server/src/metrics.rs#L100)
has no variant for an unknown or unregistered machine. NiCo cannot drop a packet
for that reason, because it never makes that test.

**Conclusion: registration gives permission and credentials. It does not control
address allocation.**

### 3.9 The one condition that refuses DHCP

NiCo refuses DHCP for one reason only: BMC suppression. Refer to
[`discover.rs#L584`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-core/src/dhcp/discover.rs#L584):

```rust
// If DHCP is suppressed for this BMC MAC, acknowledge and refuse.
// The decommission workflow polls acknowledged_at to confirm the BMC
// DHCP client has returned to the INIT state.
```

This belongs to the decommission workflow. It is not related to registration.

---

## 4. From an address to a managed host

### 4.1 Site Explorer does not scan the network

This is necessary to understand section 5.2.

Site Explorer builds its scan list from the database. It does not sweep a subnet.
Refer to
[`lib.rs#L2593`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/site-explorer/src/lib.rs#L2593):

```rust
let interfaces = db::machine_interface::find_all(&mut txn).await?;
```

A filter at
[`lib.rs#L2601`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/site-explorer/src/lib.rs#L2601)
keeps the `Underlay` rows that are a BMC type or that have no `machine_id`. It
also keeps the permitted `HostInband` rows.

**NiCo probes an address if, and only if, it has a `machine_interface` row for
it.** Usually DHCP creates that row. A Fixed reservation also creates it. Refer
to the comment at
[`lib.rs#L2588`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/site-explorer/src/lib.rs#L2588):

> Load interfaces after allocation reconciliation so this iteration also sees
> newly-created fixed reservations.

### 4.2 The exploration loop

`run_single_iteration` at
[`lib.rs#L712`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/site-explorer/src/lib.rs#L712)
takes a work lock, then calls `explore_site` at
[`lib.rs#L1039`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/site-explorer/src/lib.rs#L1039).

`explore_site` does these steps in order:

1. `check_preconditions`. This step fails the run with `MissingCredentials` if
   the site-wide BMC root credential or either UEFI default is absent.
2. `update_explored_endpoints`. This step probes each scannable interface with
   Redfish.
3. `identify_machines_to_ingest`.
4. `identify_managed_hosts`. This step pairs each host with its DPUs.
5. `create_machines`. A configuration flag controls this step.
6. Power shelves and switches. Separate flags control these steps.
7. `audit_exploration_results`.

### 4.3 The ingestion criteria

The comment at
[`lib.rs#L1058`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/site-explorer/src/lib.rs#L1058)
gives the criteria:

1. Pre-ingestion must be complete for the endpoint.
2. For a **DPU**, Site Explorer must read the MAC address of the `pf0` interface
   that the DPU gives to the host. Without this MAC address, NiCo cannot
   configure the host.
3. For a **host**, the host must be on, and infinite boot must be enabled.
   Without these conditions, NiCo cannot provision the DPU.

### 4.4 The gate

This is the only place where registration controls the result. Refer to
[`machine_creator.rs#L196`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/site-explorer/src/machine_creator.rs#L196):

```rust
/// Refuses to create a Managed Host when `expected_machine` is `None`: only hosts
/// listed in the `expected_machines` table are allowed to become Managed Hosts.
/// Already ingested hosts are not affected.
pub async fn create_managed_host(
    &self,
    explored_host: &ExploredManagedHost,
    report: &mut EndpointExplorationReport,
    expected_machine: Option<&ExpectedMachine>,
    pool: &PgPool,
) -> SiteExplorerResult<bool> {
```

The `Option<&ExpectedMachine>` parameter is the gate. `None` causes a refusal.

### 4.5 Unregistered machines stay visible

NiCo keeps a record of each endpoint that it explored but cannot ingest. A gRPC
method reports them. Refer to
[`expected_machine.rs#L402`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-core/src/handlers/expected_machine.rs#L402):

> Lists host BMC endpoints that Site Explorer has explored but whose MAC is not
> listed in any of `expected_machines`, `expected_power_shelf`, or
> `expected_switch`.

An entry with a `machine_id` that is not null is an **orphan**. NiCo ingested
that host before somebody removed its `expected_machines` row.

This method is available on the gRPC and REST interfaces. `nico-admin-cli` has no
equivalent subcommand.

### 4.6 The state machine

After the managed host exists, the state machine runs to `Ready`. Refer to
[`ingesting-hosts.md#L282`](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/provisioning/ingesting-hosts.md#L282).

```
DHCP discovery -> Preingestion -> DPU-host pairing -> DpuDiscoveringState
  -> DPUInit -> HostInit -> BomValidating / Validation -> Ready
```

During `HostInit`, the `scout` agent reports. NiCo then replaces the predicted
host ID (prefix `fm100p`) with a stable host ID (prefix `fm100h`).

---

## 5. Operational consequences

### 5.1 Bootstrap order

NiCo does not give an address to every BMC in the datacenter. It gives addresses
only to managed hosts. The site controller's own BMCs use a different network
with a different owner.

Refer to
[`network.md#L18`](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/getting-started/prerequisites/network.md#L18):

| Network | Managed by |
|---|---|
| Control Plane Management Network | **Parent datacenter via DHCP** |
| Management Network for Managed Hosts | **NiCo** |

The correct order is therefore:

1. The parent datacenter gives addresses to the site controller BMCs.
2. The site controller nodes get an operating system and a Kubernetes cluster.
   The [quick start](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/getting-started/quick-start.md#L15)
   makes this a prerequisite: *Step 2 — Prepare the Kubernetes Cluster*.
3. You deploy NiCo onto that cluster.
4. The `nico-dhcp` VIP starts.
5. Managed-host BMCs get addresses from NiCo.

The two populations do not overlap. There is no circular dependency.

If the relays point at the NiCo VIP before NiCo starts, the BMCs retry. DHCP
clients retry without a limit. They get an address when NiCo starts.

### 5.2 How to adopt a machine that has an address

There are two cases.

**Case A: the machine leased from NiCo but has no registration.**

Add the registration. The machine already has a `machine_interface` row and an
`explored_endpoints` record. The next Site Explorer pass creates the managed
host. No reboot is necessary.

**Case B: the machine has a static address from another source.**

The machine never sends DHCP to NiCo. Use a `Fixed` declaration to create the
`machine_interface` row directly. Set `fixed_ip` to the address that the BMC
already has. Refer to
[`ip-and-network-configuration.md#L178`](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/provisioning/ip-and-network-configuration.md#L178):

```json
{
  "expected_machines": [
    {
      "bmc_mac_address": "C4:5A:B1:C8:38:0D",
      "bmc_username": "root",
      "bmc_password": "<bmc-password>",
      "chassis_serial_number": "SERIAL-1",
      "interfaces": [
        {
          "mac_address": "C4:5A:B1:C8:38:0D",
          "role": "host_bmc",
          "ip_allocation": "fixed",
          "fixed_ip": "10.180.70.11",
          "network_segment_type": 2
        }
      ]
    }
  ]
}
```

Site Explorer then probes that address, because section 4.1 applies: the scan
list comes from the database, not from the network.

Four constraints apply:

1. **The address must be inside a segment that NiCo knows.** The address can be
   inside the dynamic pool of that segment. The uniqueness constraint stops the
   allocator from giving it to a different interface.
2. **The BMC MAC address and the chassis serial number must be correct.** A
   static address does not remove this requirement.
3. **The credentials must be correct.** Give the current password, not the
   factory password, if another system managed the BMC before.
4. **NiCo does not replace one Fixed address with a different Fixed address.**
   Reconciliation replaces a DHCP or SLAAC address with a Fixed reservation, but
   it does not do the other changes automatically. Refer to
   [`ip-and-network-configuration.md#L214`](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/provisioning/ip-and-network-configuration.md#L214).

> **Warning.** Registration is not a read-only operation. Ingestion takes control
> of the hardware. NiCo changes the BMC password, sets the boot order, configures
> Secure Boot, installs a DPU operating system, applies UEFI lockdown, and
> power-cycles the host. Ingestion destroys the data on the host. To see a
> machine without adoption, do not register it.

### 5.3 Drift detection

NiCo records hardware that it can see but cannot ingest. The
`get_all_unexpected_machines` method therefore answers the question: *which
machines are racked that nobody registered?*

An inventory synchronization job can use this method to compare its own records
with what NiCo observes directly.

### 5.4 Security

This design follows the NiCo principle that *the machine is untrustworthy*.

A person who connects an unauthorized machine to the OOB network gets an address
and nothing more. The machine does not enter the state machine. NiCo does not
provision it. NiCo does not connect it to a tenant network. The machine also
appears in an operator-visible list. A silent drop would be worse, because you
would lose the detection.

---

## 6. How to verify and troubleshoot

### 6.1 Verify the DHCP path

Refer to
[`ip-and-network-configuration.md#L463`](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/provisioning/ip-and-network-configuration.md#L463).

Confirm that the service has a VIP:

```bash
kubectl get svc nico-dhcp -n nico-system
```

`EXTERNAL-IP` must have a value, and `TYPE` must be `LoadBalancer`. A `<pending>`
address indicates a MetalLB problem.

Read the logs while a BMC starts:

```bash
kubectl logs -n nico-system -l app.kubernetes.io/name=nico-dhcp --tail=20 -f
```

Each DISCOVER packet must produce a line that gives the source MAC address, the
resolved segment, and either an address or a `discover_dhcp` error. A
`DeadlineExceeded` or `Unavailable` error means that the hook cannot reach
`nico-api`.

Read the Kea lease file:

```bash
kubectl exec -n nico-system deploy/nico-dhcp -- \
    cat /var/lib/kea/kea-leases4.csv | head
```

Check the relay statistics on the switch. On Cumulus or SONiC, use `show ip dhcp
relay statistics`. The count of DISCOVER packets sent must equal the count of
OFFER packets received.

### 6.2 Use the drop metric

The metric is `carbide_dhcp_dropped_requests_total`, with a `reason` label. The
event name is `dhcp_server_packet_dropped`. Refer to
[`metrics.rs#L502`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/dhcp-server/src/metrics.rs#L502).

This metric separates two faults that look the same from the host:

| Observation | Diagnosis |
|---|---|
| `reason="NonRelayedPacket"` increases | Packets arrive, but the relay does not set `giaddr`. The device bridges instead of relaying. |
| The metric does not change, and the host is absent | Packets do not arrive. There is no relay, the helper address is wrong, or a firewall blocks UDP port 67. |

Refer to Appendix C for the other drop reasons.

### 6.3 A checklist for a machine that does not appear

Use this order:

1. Does the BMC have power? Check the link light on the OOB port.
2. Does the relay forward the packets? Check the relay statistics on the switch.
3. Does NiCo receive the packets? Check the `nico-dhcp` logs and the drop metric.
4. Does the machine have an address? Check the Kea lease file.
5. Does NiCo explore the endpoint? Check `get_all_unexpected_machines`.
6. Is the machine registered? Check `nico-admin-cli em show`.
7. Are the site-wide credentials present? A run fails with `MissingCredentials`
   if they are absent.

Steps 1 to 4 test the DHCP path. Steps 5 to 7 test the registration path.

---

## Appendix A — End-to-end sequence

```
                          REGISTRATION (asynchronous)
   ┌────────────────────────────────────────────────────────────────────┐
   │ Inventory source (Launch Layer, operator, CLI)                     │
   │   POST /v2/org/{org}/nico/expected-machine[/batch]                 │
   │                              │                                     │
   │                              ▼                                     │
   │ NiCo REST handler ── cloud DB row (no BMC credentials kept)        │
   │                              │ Temporal: CreateExpectedMachine     │
   │                              ▼                                     │
   │ site-workflow ──┬─► ...OnSite ──► gRPC AddExpectedMachine ──► Core │
   │                 └─► ...OnFlow  (best effort; being removed)        │
   └────────────────────────────────────┬───────────────────────────────┘
                                        │ expected_machines table
                                        │ (allowlist + credentials)
                                        │
                          DISCOVERY (asynchronous)                     │
   ┌────────────────────────────────────────────────────────────────┐  │
   │ Person puts in the power cable                                 │  │
   │              │                                                 │  │
   │              ▼                                                 │  │
   │ BMC starts on standby power, sends DHCP DISCOVER (broadcast)   │  │
   │              │                                                 │  │
   │              ▼                                                 │  │
   │ Relay agent: broadcast ──► unicast, writes giaddr              │  │
   │              │          (no relay = NiCo drops the packet)     │  │
   │              ▼                                                 │  │
   │ Kea + nico-dhcp hooks ──gRPC discover_dhcp──► nico-api         │  │
   │              │                                                 │  │
   │              ▼                                                 │  │
   │ nico-api gives an address. It does NOT check the allowlist. ───┼──┤
   │              │                                                 │  │ reads
   │              ▼                                                 │  │ (Fixed
   │ machine_interface row exists                                   │  │  only)
   │              │                                                 │  │
   │              ▼                                                 │  │
   │ Site Explorer: machine_interface::find_all ──► Redfish probe   │  │
   │              │                                                 │  │
   │              ▼                                                 │  │
   │ identify_machines_to_ingest ──► identify_managed_hosts         │  │
   │              │                                                 │  │
   │              ▼                                                 │  │
   │ create_managed_host(expected_machine: Option<&ExpectedMachine>)◄┼──┘
   │              │                                                 │
   │      None ───┴──► REFUSED. Visible in get_all_unexpected_machines
   │      Some ───────► ManagedHost created                         │
   └────────────────────────────────────────────────────────────────┘
                                        │
                                        ▼
        DpuDiscoveringState ─► DPUInit ─► HostInit ─► Validation ─► Ready
```

## Appendix B — Code reference table

| Subject | File and line |
|---|---|
| REST create handler | [`rest-api/api/pkg/api/handler/expectedmachine.go#L89`](https://github.com/dsx-ai-factory/infra-controller/blob/main/rest-api/api/pkg/api/handler/expectedmachine.go#L89) |
| REST batch create | [`expectedmachine.go#L936`](https://github.com/dsx-ai-factory/infra-controller/blob/main/rest-api/api/pkg/api/handler/expectedmachine.go#L936) |
| Credentials not persisted | [`expectedmachine.go#L189`](https://github.com/dsx-ai-factory/infra-controller/blob/main/rest-api/api/pkg/api/handler/expectedmachine.go#L189) |
| Dual-write workflow | [`site-workflow/pkg/workflow/expectedmachine.go#L61`](https://github.com/dsx-ai-factory/infra-controller/blob/main/rest-api/site-workflow/pkg/workflow/expectedmachine.go#L61) |
| gRPC `AddExpectedMachine` call | [`site-workflow/pkg/activity/expectedmachine.go#L283`](https://github.com/dsx-ai-factory/infra-controller/blob/main/rest-api/site-workflow/pkg/activity/expectedmachine.go#L283) |
| Core `add` handler | [`crates/api-core/src/handlers/expected_machine.rs#L72`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-core/src/handlers/expected_machine.rs#L72) |
| Core `replace_all` | [`expected_machine.rs#L320`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-core/src/handlers/expected_machine.rs#L320) |
| Unexpected machines list | [`expected_machine.rs#L402`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-core/src/handlers/expected_machine.rs#L402) |
| Relay requirement | [`crates/dhcp-server/src/packet_handler.rs#L126`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/dhcp-server/src/packet_handler.rs#L126) |
| Option 82 link selection | [`packet_handler.rs#L151`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/dhcp-server/src/packet_handler.rs#L151) |
| Drop reasons | [`crates/dhcp-server/src/metrics.rs#L100`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/dhcp-server/src/metrics.rs#L100) |
| Drop metric definition | [`metrics.rs#L502`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/dhcp-server/src/metrics.rs#L502) |
| `discover_dhcp` | [`crates/api-core/src/dhcp/discover.rs#L539`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-core/src/dhcp/discover.rs#L539) |
| Expected machine lookup returns `Ok(None)` | [`discover.rs#L389`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-core/src/dhcp/discover.rs#L389) |
| BMC suppression refusal | [`discover.rs#L584`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-core/src/dhcp/discover.rs#L584) |
| Fixed preallocation | [`discover.rs#L715`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-core/src/dhcp/discover.rs#L715) |
| Allocation policies | [`crates/api-model/src/expected_machine.rs#L286`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-model/src/expected_machine.rs#L286) |
| Policy validation | [`expected_machine.rs#L311`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/api-model/src/expected_machine.rs#L311) |
| `run_single_iteration` | [`crates/site-explorer/src/lib.rs#L712`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/site-explorer/src/lib.rs#L712) |
| `explore_site` | [`lib.rs#L1039`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/site-explorer/src/lib.rs#L1039) |
| Ingestion criteria | [`lib.rs#L1058`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/site-explorer/src/lib.rs#L1058) |
| Scan list from the database | [`lib.rs#L2593`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/site-explorer/src/lib.rs#L2593) |
| Scannable interface filter | [`lib.rs#L2601`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/site-explorer/src/lib.rs#L2601) |
| `create_managed_host` gate | [`crates/site-explorer/src/machine_creator.rs#L196`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/site-explorer/src/machine_creator.rs#L196) |

### Related documentation

| Subject | Page |
|---|---|
| OOB network and relay requirements | [`prerequisites/bmc-oob-setup.md`](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/getting-started/prerequisites/bmc-oob-setup.md) |
| Address pools and network ownership | [`prerequisites/network.md`](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/getting-started/prerequisites/network.md) |
| DHCP mechanics and verification | [`provisioning/ip-and-network-configuration.md`](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/provisioning/ip-and-network-configuration.md) |
| Ingestion procedure | [`provisioning/ingesting-hosts.md`](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/provisioning/ingesting-hosts.md) |
| REST ingestion procedure | [`provisioning/ingesting-hosts-rest-api.md`](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/provisioning/ingesting-hosts-rest-api.md) |
| Interface declarations | [`provisioning/expected-machine-interfaces.md`](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/provisioning/expected-machine-interfaces.md) |
| Managed host states | [`architecture/state_machines/managedhost.md`](https://github.com/dsx-ai-factory/infra-controller/blob/main/docs/architecture/state_machines/managedhost.md) |

## Appendix C — DHCP drop reasons

These are the `DropReason` values from
[`metrics.rs#L100`](https://github.com/dsx-ai-factory/infra-controller/blob/main/crates/dhcp-server/src/metrics.rs#L100).
They become the `reason` label on `carbide_dhcp_dropped_requests_total`.

**Relay and addressing:**

| Reason | Meaning |
|---|---|
| `NonRelayedPacket` | `giaddr` is `0.0.0.0` or a broadcast address. No relay processed the packet. |
| `MissingRelayCode` | A relay agent option is present but incomplete. |
| `NotMyPacket` | The server identifier belongs to a different DHCP server. |
| `AddressParseError` | An address in the packet is not valid. |

**Packet structure:**

| Reason | Meaning |
|---|---|
| `UnknownPacket` | The BOOTP operation code is not known. |
| `PacketDecodeFailure` / `PacketEncodeFailure` | The packet is malformed. |
| `MissingOption` / `MissingOptionV6` | A necessary DHCP option is absent. |
| `UnhandledMessageType` / `UnhandledMessageTypeV6` | The message type has no handler. |
| `VendorClassParseError` | Option 60 is malformed. |
| `DhcpDeclineMessage` | The client sent a DECLINE message. |

**IPv6:**

| Reason | Meaning |
|---|---|
| `MalformedDuid` | The DUID is not valid. |
| `NoMacNoOption79` | There is no MAC address and no Option 79 (client link-layer address). |
| `NestedRelayV6` | The packet has nested Relay-Forward messages. |
| `RelayHopCountExceededV6` | The relay hop count is too high. |

**Service:**

| Reason | Meaning |
|---|---|
| `UpstreamApiError` | The hook cannot reach `nico-api`. Check the VIP and the TLS material. |
| `ConfigParseFailure` | The configuration is not valid. |
| `MultipleInterfaces` | More than one interface was given. Only one is supported. |
| `IoError`, `Utf8Error`, `InvalidInput`, `GenericError`, `MissingArgument` | General failures. |

> **Note.** There is no drop reason for an unregistered or unknown machine.
> Section 3.8 explains why.
