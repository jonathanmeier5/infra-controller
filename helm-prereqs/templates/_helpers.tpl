{{/*
Generate an ed25519 SSH host private key (PKCS8 PEM format).
Reuses the existing secret on helm upgrade (idempotent).
No Job required — avoids network dependency for apk.
*/}}
{{- define "prereqs.sshPrivateKey" -}}
{{- $existing := (lookup "v1" "Secret" .Values.namespace "ssh-host-key") -}}
{{- if $existing -}}
  {{- index $existing.data "ssh_host_ed25519_key" | b64dec -}}
{{- else -}}
  {{- genPrivateKey "ed25519" -}}
{{- end -}}
{{- end -}}

{{/*
Render one vault.siteCredentials entry as a Vault KV UsernamePassword payload.
Takes a dict with `key` (values path for error messages), `cred` and
`requireUsername`. Fails the render unless both fields are quoted strings.
*/}}
{{- define "prereqs.siteCredentialJson" -}}
{{- $password := required (printf "vault.siteCredentials.%s.password must be set when vault.siteCredentials.enabled is true" .key) .cred.password -}}
{{- if not (kindIs "string" $password) -}}
{{- fail (printf "vault.siteCredentials.%s.password must be a quoted string" .key) -}}
{{- end -}}
{{- $username := .cred.username -}}
{{- if .requireUsername -}}
{{- $username = required (printf "vault.siteCredentials.%s.username must be set when vault.siteCredentials.enabled is true" .key) $username -}}
{{- else if kindIs "invalid" $username -}}
{{- $username = "" -}}
{{- end -}}
{{- if not (kindIs "string" $username) -}}
{{- fail (printf "vault.siteCredentials.%s.username must be a quoted string" .key) -}}
{{- end -}}
{{- dict "UsernamePassword" (dict "username" $username "password" $password) | toJson -}}
{{- end -}}
