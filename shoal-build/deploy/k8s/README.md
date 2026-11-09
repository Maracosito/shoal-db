# Shoal raw Kubernetes manifests

Plain manifests for a small Shoal deployment: API server (2 replicas, with
local-disk cache), background worker (1 replica), config, and an optional
ingress. For templated installs, prefer the Helm chart in
[`deploy/helm/shoal`](../helm/shoal).

These manifests assume an **external S3-compatible object store** (AWS S3,
GCS interoperability mode, MinIO, etc.). Shoal compute is stateless — all
durable state lives in the bucket — so pods use ephemeral `emptyDir` cache
volumes and can be rescheduled freely.

## Install

1. Build/push the image (or use a published one) and update the `image:`
   fields in `30-api.yaml` and `40-worker.yaml`.

2. Create the secret with real credentials (do **not** apply the example
   file as-is in production):

   ```sh
   cp 10-secret.example.yaml 10-secret.yaml
   $EDITOR 10-secret.yaml     # set the root API key + S3 credentials
   ```

3. Adjust the bucket/endpoint/region in `20-configmap.yaml`.

4. Apply:

   ```sh
   kubectl apply -f 00-namespace.yaml
   kubectl apply -f 10-secret.yaml
   kubectl apply -f 20-configmap.yaml -f 30-api.yaml -f 40-worker.yaml
   # optional:
   kubectl apply -f 50-ingress.yaml
   ```

5. Verify:

   ```sh
   kubectl -n shoal get pods
   kubectl -n shoal port-forward svc/shoal-api 8080:8080
   curl -H "Authorization: Bearer $SHOAL_ROOT_API_KEY" http://localhost:8080/healthz
   ```

## Notes

- The API pods expose Prometheus metrics at `/metrics` on the service port
  and carry `prometheus.io/*` scrape annotations.
- Cache volumes are `emptyDir` with a `sizeLimit`; tune `sizeLimit` and
  `SHOAL_CACHE_DISK_BYTES` together (keep the env var below the volume
  limit). For consistently warm caches across restarts, switch to a
  `StatefulSet` with PVCs — see `docs/deployment.md`.
- See `docs/deployment.md` for the full environment-variable reference and
  production hardening checklist.
