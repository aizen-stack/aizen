# Aizen on Kubernetes

Read this before applying anything. The manifests here are deliberately shaped against what
Kubernetes is usually *for*, and the reasons are worth understanding — most of them are properties of
the Telegram/Discord APIs and of the daemon's design, not things a manifest can tune away.

## The one rule: exactly one replica

**`replicas: 1`, forever.** Telegram permits exactly one `getUpdates` poller per bot token and
answers a second one with `409 Conflict` (the daemon logs this — see
`src/hostbot/platforms/telegram.rs`). So `replicas: 2` is not "twice the throughput", it is one
healthy pod and one pod in a permanent crash-retry loop.

Scaling out doesn't help even if you shard bots across pods: `run_daemon` processes messages
**serially** by design, because the destructive-op approval route is pinned per turn and serial
execution is what keeps it race-free. And one daemon already hosts many bots in one process
(`/addbot`), so splitting them across pods undoes work the code does on purpose.

If you need more concurrency, the answer is more bots on the one daemon, not more daemons.

## Why StatefulSet and not Deployment

Every on-disk store in `~/.aizen` is guarded by a **file lock** (`RepoTxnLock`, `flock`/`LockFileEx`).
Those are only meaningful within one host, which forces `ReadWriteOnce` — a shared `ReadWriteMany`
volume with two writers would silently corrupt state, because the locks can't see each other across
nodes.

The update strategy follows from the same 409. A rolling update that briefly ran old and new pods
together would create the collision *during the rollout* — so `podManagementPolicy: OrderedReady` is
set, and at one replica RollingUpdate terminates the old pod fully before creating the new one. The
effect is Recreate semantics; the manifest states it explicitly so the constraint survives the next
edit.

## Should you use Kubernetes at all?

Probably not, if the goal is just "keep the bot alive". `aizen serve --install --user --now` writes a
systemd unit that already does auto-restart, start-on-boot, and survive-logout. With one replica, a
Service that exists only to satisfy the StatefulSet schema, no Ingress, and no horizontal scaling,
this StatefulSet is a more expensive way to get the same thing.

It's worth it when you already run a cluster and want aizen managed alongside everything else —
one place for secrets, node failover, and unified logging.

## Files

| File | What it is |
| --- | --- |
| `namespace.yaml` | The `aizen` namespace. |
| `secret.example.yaml` | Template for the tokens. **Do not commit a filled-in copy.** |
| `configmap.yaml` | Non-secret settings (endpoint URL, model, timeouts). |
| `statefulset.yaml` | The daemon itself, with probes, security context, and its PVC. |
| `service.yaml` | Headless, portless — exists only because `serviceName` requires it. |
| `networkpolicy.yaml` | Denies all ingress and all private egress (incl. cloud metadata). |
| `kustomization.yaml` | Ties them together: `kubectl apply -k .` — everything except the Secret. |

## Install

```bash
# 1. Namespace
kubectl apply -f namespace.yaml

# 2. Secrets — from literals, so nothing sensitive is ever written to a file
kubectl -n aizen create secret generic aizen-secrets \
  --from-literal=AIZEN_API_KEY='sk-...' \
  --from-literal=AIZEN_TELEGRAM_TOKEN='123456:ABC-...' \
  --from-literal=AIZEN_TAVILY_API_KEY='tvly-...'

# 3. Review the endpoint + model, then apply the rest
$EDITOR configmap.yaml
kubectl apply -k .

# 4. First run prints a pairing code — send it to your bot from Telegram to claim ownership
kubectl -n aizen logs -f statefulset/aizen
```

## Verify

```bash
kubectl -n aizen get pod                        # Running, 1/1
kubectl -n aizen exec statefulset/aizen -- aizen serve --health
# → aizen serve [telegram] healthy — idle 7s ago (pid 1)
```

## Health probes

There is no HTTP endpoint to probe, and that is intentional: the daemon listens on nothing at all,
which is what lets it run behind NAT with no public URL. Adding a port purely so an orchestrator
could `GET /healthz` would forfeit that and hand out an unauthenticated surface. So both probes are
`exec` probes reading a heartbeat file (`src/hostbot/health.rs`).

The heartbeat carries a **state**, not just a timestamp, because an agent turn legitimately takes
minutes — a build, a test suite, a long tool chain. A plain "last touched" clock cannot tell *wedged*
from *working hard*, so a probe tuned to catch the first would kill the second mid-build. `idle` must
refresh every 15s; `busy` gets 30 minutes (`AIZEN_HEALTH_MAX_BUSY_SECS`). Raise it if your turns run
longer than that.

## Security

Two things need saying plainly, because they are properties of what aizen *is*:

**The agent has a shell.** With `/approval yolo` it runs model-chosen commands without asking. Inside
a pod that means the pod's entire reach is the agent's reach. That is exactly why
`automountServiceAccountToken: false` is set — otherwise the agent can read
`/var/run/secrets/kubernetes.io/serviceaccount/token` with a plain file read and talk to the
apiserver as the pod. No network policy or SSRF guard is relevant to that path; not mounting the
token is the fix.

**`AIZEN_ALLOW_PRIVATE_NET` is a bigger switch than it looks.** aizen's SSRF floor
(`src/core/net_guard.rs`) refuses private, loopback, and link-local targets, which in a cluster
includes every ClusterIP — so `web_fetch` cannot reach your internal services. Setting the flag
unblocks them, and also unblocks `169.254.169.254`, your node's cloud metadata endpoint and its
credentials. Prefer a NetworkPolicy that permits the specific services you need and denies the
metadata CIDR.

Also applied: non-root (uid 10001), all capabilities dropped, `allowPrivilegeEscalation: false`, and
`seccompProfile: RuntimeDefault`.

`readOnlyRootFilesystem` is **not** set, and cannot be: the agent writes to `/work`, and cargo, npm,
and pip all want a writable temp dir. Enabling it would need a writable `emptyDir` at every one of
those paths, which buys little once `/work` is writable anyway.

## Backup

`~/.aizen` is the whole state: `cli-config.json` (including the `allowed_chat_ids` that pairing
writes), `hostbot/bots.json` (sub-bot tokens), `hostbot/sessions/` (per-chat context), `cli-memory/`
(memory + codebase index). Losing the PVC means re-pairing and losing all memory.

```bash
kubectl -n aizen exec statefulset/aizen -- tar cz -C /home/aizen .aizen > aizen-home-backup.tgz
```

## Upgrade

```bash
kubectl -n aizen set image statefulset/aizen aizen=your-registry/aizen:v0.5.1
```

A 1-replica StatefulSet rolls by terminating the old pod fully before creating the new one — which is
required here, not merely tidy: two pods polling the same token at once is the 409 collision, so an
overlapping rollout would break the very thing it was updating. That means a short gap where the bot
is unreachable, the price of the one-poller rule. Messages sent during the gap are not lost: Telegram
queues them (up to 24h) and the new pod picks them up on its first poll.

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| `CrashLoopBackOff`, logs show `409 Conflict` | Another poller has the token — a second replica, or a daemon still running elsewhere (your laptop, a VPS). One token, one poller. |
| Liveness restarts during long turns | A turn legitimately exceeds 30 min. Raise `AIZEN_HEALTH_MAX_BUSY_SECS` in the ConfigMap. |
| Pod `Pending` | No `ReadWriteOnce` PVC could be bound. `volumeClaimTemplates` sets no `storageClassName`, so the cluster default is used — add one if that default is RWX-only or absent. |
| Bot ignores you | Your chat id isn't in `allowed_chat_ids`. Send the pairing code from the logs. |
| `web_fetch` fails on internal URLs | The SSRF floor. See the security note above before setting the flag. |
| Auth fails right after an apply | `kubectl apply -f deploy/k8s/` (with `-f`, not `-k`) also applies `secret.example.yaml` and overwrites your tokens with `REPLACE_ME`. Use `-k`, which excludes it. |
