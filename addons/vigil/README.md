# Vigil

Local Home Assistant add-on package for the Vigil substrate runtime.

## Supervisor API access

The manifest requests `hassio_api: true` with `hassio_role: default`, so this
add-on's own container can read Home Assistant's Supervisor API and write its
own options with its own credentials. Without it, a value pushed from a
management server never lands on the add-on's options page — it keeps
showing whatever the operator last saw, which means the surface an operator
trusts most would be telling them something that stopped being true. The
ordinary `default` role already permits an add-on to write its own options —
measured empirically against a live Supervisor, where a complete self-options
write using this container's own token succeeded at `default`. Nothing
broader is requested: a role that reaches every installed add-on's
Supervisor-managed state would be more than this feature needs.

## Distributed compute (fabric)

Join this node to another Vigil node's distributed compute fabric (paste the
join ticket printed in its status/doctor output into the `fabric_ticket`
option, or turn this node into the join point with `fabric_hub`) and detection
work automatically balances across every joined node under queue pressure —
an ephemeral compressed clip of a motion event moves to same-tenant machines
by default whenever this node's own detector queue is under pressure and a
joined node has spare capacity. The clip is deleted once detection completes;
it is never sent to any third party, never through a hub, always node-to-node
between machines you enrolled yourself. Turn this off per camera with the
`fabric_allow_frame_offload` option (default on) — this node still joins the
fabric and can still accept offloaded work from other nodes, it simply never
sends this camera's own clips elsewhere.
