//go:build headless

// Headless probe — никакого браузера, чистый HTTP.
// Берёт куки .claude.ai из CookieBridge Hub, POST'ит на /v1/oauth/<org>/authorize,
// получает code, обменивает на токены через /v1/oauth/token.
//
// Запуск: go run -tags headless headless.go <email> [profile]

package main

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"

	"probe-oauth/cookiebridge"
)

const (
	clientID    = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
	scopes      = "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"
	redirectURI = "https://platform.claude.com/oauth/code/callback"
	userAgent   = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36"
)

func b64url(b []byte) string {
	return strings.TrimRight(base64.URLEncoding.EncodeToString(b), "=")
}

func genPKCE() (verifier, challenge, state string) {
	v := make([]byte, 32)
	_, _ = rand.Read(v)
	verifier = b64url(v)
	sum := sha256.Sum256([]byte(verifier))
	challenge = b64url(sum[:])
	s := make([]byte, 32)
	_, _ = rand.Read(s)
	state = b64url(s)
	return
}

// Build Cookie header value from list of CB cookies.
func buildCookieHeader(cks []cookiebridge.Cookie) string {
	parts := make([]string, 0, len(cks))
	for _, c := range cks {
		// Filter out cookies for sub-paths or wrong domains — claude.ai expects
		// cookies whose domain matches .claude.ai or claude.ai.
		if !(strings.EqualFold(c.Domain, ".claude.ai") || strings.EqualFold(c.Domain, "claude.ai")) {
			continue
		}
		parts = append(parts, c.Name+"="+c.Value)
	}
	return strings.Join(parts, "; ")
}

// Find a cookie by name (case-sensitive).
func findCookie(cks []cookiebridge.Cookie, name string) string {
	for _, c := range cks {
		if c.Name == name {
			return c.Value
		}
	}
	return ""
}

// Find org_uuid from lastActiveOrg cookie.
func findOrgUUID(cks []cookiebridge.Cookie) string {
	return findCookie(cks, "lastActiveOrg")
}

// cookiesForAccount picks the snapshot matching either Email==key or
// AccountLabel==key (claude.ai snapshots have label="Pavel"/"Rumo" without email).
func cookiesForAccount(snaps []cookiebridge.Snapshot, key string) []cookiebridge.Cookie {
	type k struct{ name, domain, path string }
	seen := map[k]bool{}
	var out []cookiebridge.Cookie
	for _, s := range snaps {
		if s.State == "logged_out" {
			continue
		}
		match := strings.EqualFold(s.Email, key) ||
			strings.EqualFold(s.AccountLabel, key) ||
			strings.EqualFold(s.AccountID, key)
		if !match {
			continue
		}
		for _, c := range s.Cookies {
			kk := k{c.Name, c.Domain, c.Path}
			if seen[kk] {
				continue
			}
			seen[kk] = true
			out = append(out, c)
		}
	}
	return out
}

// POST /v1/oauth/<org>/authorize with the JSON body Anthropic expects.
func authorizeOnClaude(orgUUID string, body authorizeBody, cookieHeader, deviceID, anonID string) (string, error) {
	endpoint := fmt.Sprintf("https://claude.ai/v1/oauth/%s/authorize", orgUUID)
	bodyJSON, _ := json.Marshal(body)

	req, _ := http.NewRequest("POST", endpoint, bytes.NewReader(bodyJSON))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "*/*")
	req.Header.Set("Origin", "https://claude.ai")
	req.Header.Set("Referer", buildAuthorizeReferer(body))
	req.Header.Set("User-Agent", userAgent)
	req.Header.Set("Anthropic-Client-Platform", "web_claude_ai")
	req.Header.Set("Anthropic-Client-Version", "unknown")
	if anonID != "" {
		req.Header.Set("Anthropic-Anonymous-Id", anonID)
	}
	if deviceID != "" {
		req.Header.Set("Anthropic-Device-Id", deviceID)
	}
	req.Header.Set("Cookie", cookieHeader)
	req.Header.Set("sec-fetch-dest", "empty")
	req.Header.Set("sec-fetch-mode", "cors")
	req.Header.Set("sec-fetch-site", "same-origin")
	req.Header.Set("Accept-Encoding", "identity") // no gzip — easier to debug

	client := &http.Client{Timeout: 30 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		return "", fmt.Errorf("POST authorize: %w", err)
	}
	defer resp.Body.Close()
	respBody, _ := io.ReadAll(resp.Body)

	fmt.Printf("[*] authorize HTTP %d\n", resp.StatusCode)
	fmt.Printf("[*] response body: %s\n", trim(string(respBody), 800))
	if resp.StatusCode != 200 {
		return "", fmt.Errorf("authorize HTTP %d: %s", resp.StatusCode, trim(string(respBody), 200))
	}

	var out struct {
		Code              string `json:"code"`
		AuthorizationCode string `json:"authorization_code"`
		RedirectURL       string `json:"redirect_url"`
		RedirectURI       string `json:"redirect_uri"`
	}
	if err := json.Unmarshal(respBody, &out); err != nil {
		return "", fmt.Errorf("parse authorize body: %w", err)
	}

	code := out.Code
	if code == "" {
		code = out.AuthorizationCode
	}
	if code == "" {
		// Sometimes the code lives inside redirect_url as ?code=...
		ru := out.RedirectURL
		if ru == "" {
			ru = out.RedirectURI
		}
		if ru != "" {
			if u, err := url.Parse(ru); err == nil {
				code = u.Query().Get("code")
			}
		}
	}
	return code, nil
}

type authorizeBody struct {
	ResponseType        string `json:"response_type"`
	ClientID            string `json:"client_id"`
	CodeChallenge       string `json:"code_challenge"`
	CodeChallengeMethod string `json:"code_challenge_method"`
	OrganizationUUID    string `json:"organization_uuid"`
	RedirectURI         string `json:"redirect_uri"`
	Scope               string `json:"scope"`
	State               string `json:"state"`
}

func buildAuthorizeReferer(b authorizeBody) string {
	q := url.Values{}
	q.Set("code", "true")
	q.Set("client_id", b.ClientID)
	q.Set("response_type", b.ResponseType)
	q.Set("redirect_uri", b.RedirectURI)
	q.Set("scope", b.Scope)
	q.Set("code_challenge", b.CodeChallenge)
	q.Set("code_challenge_method", b.CodeChallengeMethod)
	q.Set("state", b.State)
	return "https://claude.ai/oauth/authorize?" + q.Encode()
}

// exchangeToken sends JSON body (which is what real CLI does) to console.anthropic.com.
// Form-encoded gives "Invalid request format" — Anthropic expects JSON here.
func exchangeToken(code, verifier, state string) ([]byte, int, error) {
	endpoint := "https://console.anthropic.com/v1/oauth/token"
	bodyJSON, _ := json.Marshal(map[string]string{
		"grant_type":    "authorization_code",
		"code":          code,
		"code_verifier": verifier,
		"client_id":     clientID,
		"redirect_uri":  redirectURI,
		"state":         state,
	})
	fmt.Printf("[*] POST %s body=%s\n", endpoint, string(bodyJSON))

	req, _ := http.NewRequest("POST", endpoint, bytes.NewReader(bodyJSON))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", userAgent)
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, 0, err
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	fmt.Printf("    HTTP %d body: %s\n", resp.StatusCode, trim(string(body), 600))
	if resp.StatusCode != 200 {
		return body, resp.StatusCode, fmt.Errorf("HTTP %d", resp.StatusCode)
	}
	return body, 200, nil
}

func trim(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}

func main() {
	if len(os.Args) < 2 {
		fmt.Println("usage: headless <account_key> [profile]")
		fmt.Println("  account_key: email OR accountLabel (e.g. \"Pavel\" or \"Rumo\")")
		fmt.Println("  profile    : Chrome profile in CookieBridge (default \"upsoftt\")")
		os.Exit(2)
	}
	accountKey := os.Args[1]
	profile := "upsoftt"
	if len(os.Args) > 2 {
		profile = os.Args[2]
	}
	fmt.Printf("[*] account_key: %s\n", accountKey)
	fmt.Printf("[*] profile    : %s\n", profile)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
	defer cancel()
	cli, err := cookiebridge.Start(ctx, cookiebridge.Config{
		ID:          "claudemonitor-probe",
		DisplayName: "ClaudeMonitor OAuth Probe",
		Domains:     []string{".claude.ai", "claude.ai"},
		Profile:     profile,
		PersistDir:  ".cb_state",
	})
	if err != nil {
		log.Fatalf("[!] cb Start: %v", err)
	}
	if err := cli.WaitReady(ctx); err != nil {
		log.Fatalf("[!] cb WaitReady: %v", err)
	}
	fmt.Println("[+] hub ready")

	snaps, err := cli.FetchSnapshots(ctx, []string{".claude.ai", "claude.ai"}, 10, true)
	if err != nil {
		log.Fatalf("[!] cb fetch: %v", err)
	}
	fmt.Printf("[*] got %d snapshots\n", len(snaps))
	for _, s := range snaps {
		fmt.Printf("    snap domain=%s label=%q email=%q accountId=%q state=%s cookies=%d\n",
			s.Domain, s.AccountLabel, s.Email, s.AccountID, s.State, len(s.Cookies))
	}

	// Strategy: try to match by label/email/accountId; if that yields nothing,
	// fall back to ALL snapshots from this profile (works when hub doesn't
	// classify claude.ai snapshots into per-account labels).
	merged := cookiesForAccount(snaps, accountKey)
	if len(merged) == 0 {
		fmt.Printf("[*] no per-account match; merging ALL %d snapshots\n", len(snaps))
		type k struct{ name, domain, path string }
		seen := map[k]bool{}
		for _, s := range snaps {
			if s.State == "logged_out" {
				continue
			}
			for _, c := range s.Cookies {
				kk := k{c.Name, c.Domain, c.Path}
				if seen[kk] {
					continue
				}
				seen[kk] = true
				merged = append(merged, c)
			}
		}
	}
	if len(merged) == 0 {
		log.Fatalf("[!] no cookies after fallback merge")
	}
	fmt.Printf("[+] merged %d cookies for %s\n", len(merged), accountKey)
	cookieNames := []string{}
	for _, c := range merged {
		cookieNames = append(cookieNames, c.Name)
	}
	fmt.Printf("    cookie names: %v\n", cookieNames)

	orgUUID := findOrgUUID(merged)
	if orgUUID == "" {
		log.Fatalf("[!] no lastActiveOrg cookie — can't determine organization_uuid")
	}
	fmt.Printf("[*] organization_uuid (from lastActiveOrg): %s\n", orgUUID)

	deviceID := findCookie(merged, "anthropic-device-id")
	anonID := findCookie(merged, "ajs_anonymous_id")
	fmt.Printf("[*] device-id : %s\n", deviceID)
	fmt.Printf("[*] anon-id   : %s\n", anonID)

	verifier, challenge, state := genPKCE()
	body := authorizeBody{
		ResponseType:        "code",
		ClientID:            clientID,
		CodeChallenge:       challenge,
		CodeChallengeMethod: "S256",
		OrganizationUUID:    orgUUID,
		RedirectURI:         redirectURI,
		Scope:               scopes,
		State:               state,
	}
	cookieHeader := buildCookieHeader(merged)
	fmt.Printf("[*] cookie header length: %d bytes\n", len(cookieHeader))

	code, err := authorizeOnClaude(orgUUID, body, cookieHeader, deviceID, anonID)
	if err != nil {
		log.Fatalf("[!] authorize: %v", err)
	}
	if code == "" {
		log.Fatalf("[!] no code in authorize response")
	}
	fmt.Printf("[+] got code: %s...\n", code[:min(len(code), 24)])

	tokenBody, _, err := exchangeToken(code, verifier, state)
	if err != nil {
		log.Fatalf("[!] exchange: %v", err)
	}

	var payload map[string]any
	_ = json.Unmarshal(tokenBody, &payload)
	masked := map[string]any{}
	for k, v := range payload {
		if s, ok := v.(string); ok && len(s) > 24 {
			masked[k] = s[:18] + "..."
		} else {
			masked[k] = v
		}
	}
	pretty, _ := json.MarshalIndent(masked, "", "  ")
	fmt.Printf("[+] tokens (masked):\n%s\n", string(pretty))

	out := "headless_result.json"
	full, _ := json.MarshalIndent(payload, "", "  ")
	if err := os.WriteFile(out, full, 0o644); err != nil {
		fmt.Printf("[!] write result: %v\n", err)
	} else {
		fmt.Printf("[*] saved → %s\n", out)
	}
}


func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}
