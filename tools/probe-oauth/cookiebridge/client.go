// Package cookiebridge integrates with the CookieBridge Hub in PULL MODE:
// no inbound webhook port is opened — instead the daemon long-polls
// /consumers/:id/cookies/fetch on demand with HMAC-signed requests.
//
// Wire protocol (authoritative source: hub/internal/hmac/request.go):
//
//	canonical = ts + "\n" + nonce + "\n" + METHOD + "\n" + PATH + "\n" + sha256_hex(body)
//	signature = "sha256=" + hex(HMAC-SHA256(consumerSecret, canonical))
//
// The signature plus ts/nonce/sig live in X-CB-Timestamp / X-CB-Nonce /
// X-CB-Signature headers on every authenticated request.
//
// Persistence: the consumer secret returned by /register is stored at
// {PersistDir}/cb_secret.json (0600). On daemon restart we load that file
// and skip re-registration. If it's missing on a hub that already approved
// us (HTTP 409 from /register), we surface a clear error pointing to the
// admin UI to revoke and re-approve.
package cookiebridge

import (
	"bytes"
	"context"
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"sync"
	"time"
)

// Cookie matches the fields hub returns in snapshots and the fields chromedp
// expects when injecting cookies into a Chromium session.
type Cookie struct {
	Name     string `json:"name"`
	Value    string `json:"value"`
	Domain   string `json:"domain"`
	Path     string `json:"path"`
	Secure   bool   `json:"secure"`
	HTTPOnly bool   `json:"httpOnly"`
	SameSite string `json:"sameSite"`
	Expires  int64  `json:"expires"`
}

// Snapshot mirrors the fetch response shape from
// hub/internal/server/handlers_consumer_fetch.go.
type Snapshot struct {
	ID            int64    `json:"id"`
	BrowserType   string   `json:"browserType"`
	ProfileLabel  string   `json:"profileLabel"`
	AccountLabel  string   `json:"accountLabel"`
	AccountID     string   `json:"accountId"`
	Email         string   `json:"email"`
	AuthuserIndex int      `json:"authuserIndex"`
	Domain        string   `json:"domain"`
	State         string   `json:"state"`
	LastUpdatedAt int64    `json:"lastUpdatedAt"`
	Cookies       []Cookie `json:"cookies"`
}

// Payload is kept for callers that already use it (browser/session.go,
// gen/*.go). Synthesised from the freshest Snapshot on demand.
type Payload struct {
	ProfileLabel string   `json:"profileLabel"`
	Domain       string   `json:"domain"`
	Cookies      []Cookie `json:"cookies"`
	Timestamp    int64    `json:"timestamp"`
	AccountID    string   `json:"accountId"`
	Email        string   `json:"email"`
}

type Config struct {
	HubURL      string   // default http://127.0.0.1:19280
	ID          string   // default "gemini-mcp"
	DisplayName string   // shown in TrayConsole approval prompt
	Domains     []string // declared in manifest; default fetch domains
	KeyCookies  []string // optional, hub may use to delay pushes that miss them
	Profile     string   // pull-mode REQUIRES a concrete profile label
	PersistDir  string   // default %LOCALAPPDATA%/GeminiMCP or ~/.config/gemini-mcp
}

// Client is the long-lived consumer handle.
type Client struct {
	cfg Config

	mu       sync.RWMutex
	secret   []byte // HMAC key
	cache    map[string]cacheEntry
	ready    bool
	readyCh  chan struct{}
	cancelReg context.CancelFunc
}

type cacheEntry struct {
	snap    Snapshot
	fetched time.Time
}

// cacheTTL is the in-memory snapshot lifetime. Two minutes is plenty for the
// duration of one orchestration (image generation, music, etc.) while staying
// well below cookie expiry windows. Anything over a minute also means
// back-to-back tool calls don't pummel the hub.
const cacheTTL = 2 * time.Minute

// Start loads an existing secret from disk and is immediately ready, or kicks
// off /register in a goroutine that blocks up to 5 min on user approval —
// callers should WaitReady before issuing fetches in that case.
func Start(ctx context.Context, cfg Config) (*Client, error) {
	if cfg.HubURL == "" {
		cfg.HubURL = "http://127.0.0.1:19280"
	}
	if cfg.ID == "" {
		cfg.ID = "gemini-mcp"
	}
	if cfg.Profile == "" {
		return nil, fmt.Errorf("cookiebridge: Profile is required in pull-mode (set GEMINI_MCP_PROFILE to a concrete Chrome profile label e.g. \"upsoft.mail\"; \"*\" is rejected by the hub)")
	}
	if cfg.PersistDir == "" {
		cfg.PersistDir = DefaultPersistDir()
	}
	if err := os.MkdirAll(cfg.PersistDir, 0o755); err != nil {
		return nil, fmt.Errorf("mkdir persist dir: %w", err)
	}

	c := &Client{
		cfg:     cfg,
		cache:   map[string]cacheEntry{},
		readyCh: make(chan struct{}),
	}

	if sec, err := loadSecret(c.secretPath()); err == nil {
		c.secret = sec
		c.markReady()
		fmt.Fprintf(os.Stderr, "[cookiebridge] loaded existing secret for consumer %q\n", c.cfg.ID)
		return c, nil
	}

	regCtx, cancel := context.WithCancel(context.Background())
	c.cancelReg = cancel
	go func() {
		fmt.Fprintf(os.Stderr, "[cookiebridge] no local secret found — registering with hub %s as %q (approve in TrayConsole popup)…\n", cfg.HubURL, cfg.ID)
		secret, err := c.register(regCtx)
		if err != nil {
			fmt.Fprintf(os.Stderr, "[cookiebridge] register failed: %v\n", err)
			return
		}
		if err := saveSecret(c.secretPath(), secret); err != nil {
			fmt.Fprintf(os.Stderr, "[cookiebridge] save secret failed: %v\n", err)
			return
		}
		c.mu.Lock()
		c.secret = secret
		c.mu.Unlock()
		c.markReady()
		fmt.Fprintf(os.Stderr, "[cookiebridge] registered, secret persisted to %s\n", c.secretPath())
	}()

	return c, nil
}

func (c *Client) Stop(_ context.Context) error {
	if c.cancelReg != nil {
		c.cancelReg()
	}
	return nil
}

func (c *Client) Ready() bool {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return c.ready
}

func (c *Client) WaitReady(ctx context.Context) error {
	select {
	case <-c.readyCh:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

func (c *Client) markReady() {
	c.mu.Lock()
	if c.ready {
		c.mu.Unlock()
		return
	}
	c.ready = true
	c.mu.Unlock()
	close(c.readyCh)
}

// register POSTs the manifest and waits up to 5 min for the user's TrayConsole
// approval (the hub holds the connection open until decision).
func (c *Client) register(ctx context.Context) ([]byte, error) {
	manifest := map[string]interface{}{
		"id":            c.cfg.ID,
		"displayName":   c.cfg.DisplayName,
		"domains":       c.cfg.Domains,
		"keyCookies":    c.cfg.KeyCookies,
		"profiles":      []string{c.cfg.Profile},
		"receiver":      map[string]string{"url": ""},
		"schemaVersion": "1.0",
		"mode":          "pull",
	}
	body, _ := json.Marshal(manifest)

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.cfg.HubURL+"/register", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")

	httpClient := &http.Client{Timeout: 6 * time.Minute}
	resp, err := httpClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("POST /register: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)

	if resp.StatusCode == 409 {
		return nil, fmt.Errorf("hub already has consumer %q approved but local secret was lost. Revoke it via http://localhost:19280/admin/cookies, then restart the daemon to re-register", c.cfg.ID)
	}
	if resp.StatusCode == 403 {
		return nil, fmt.Errorf("user denied approval in TrayConsole")
	}
	if resp.StatusCode == 408 {
		return nil, fmt.Errorf("approval timed out (5 min) — no one clicked Approve")
	}
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("POST /register: HTTP %d: %s", resp.StatusCode, truncate(string(raw), 300))
	}

	// Hub field name has varied across versions: accept consumerSecret/secret + consumerId/id.
	var out struct {
		ConsumerID     string `json:"consumerId"`
		ConsumerSecret string `json:"consumerSecret"`
		ID             string `json:"id"`
		Secret         string `json:"secret"`
	}
	if err := json.Unmarshal(raw, &out); err != nil {
		return nil, fmt.Errorf("parse /register response: %w (body=%s)", err, truncate(string(raw), 200))
	}
	sec := out.ConsumerSecret
	if sec == "" {
		sec = out.Secret
	}
	if sec == "" {
		return nil, fmt.Errorf("/register returned empty secret (body=%s)", truncate(string(raw), 200))
	}
	return []byte(sec), nil
}

// FetchSnapshots calls POST /consumers/:id/cookies/fetch with HMAC.
func (c *Client) FetchSnapshots(ctx context.Context, domains []string, timeoutSec int, refresh bool) ([]Snapshot, error) {
	c.mu.RLock()
	secret := c.secret
	c.mu.RUnlock()
	if secret == nil {
		return nil, fmt.Errorf("cookiebridge not ready (no secret yet — registration pending)")
	}
	if timeoutSec <= 0 {
		timeoutSec = 8
	}
	if timeoutSec > 15 {
		timeoutSec = 15
	}
	body := map[string]interface{}{
		"domains":    domains,
		"timeoutSec": timeoutSec,
		"refresh":    refresh,
	}
	raw, _ := json.Marshal(body)

	ts := time.Now().Unix()
	nonce := newNonce()
	path := "/consumers/" + c.cfg.ID + "/cookies/fetch"
	sig := signRequest(secret, ts, nonce, http.MethodPost, path, raw)

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.cfg.HubURL+path, bytes.NewReader(raw))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("X-CB-Timestamp", strconv.FormatInt(ts, 10))
	req.Header.Set("X-CB-Nonce", nonce)
	req.Header.Set("X-CB-Signature", sig)

	httpClient := &http.Client{Timeout: time.Duration(timeoutSec+5) * time.Second}
	resp, err := httpClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("POST %s: %w", path, err)
	}
	defer resp.Body.Close()
	rb, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("POST %s: HTTP %d: %s", path, resp.StatusCode, truncate(string(rb), 300))
	}
	var out struct {
		Snapshots []Snapshot `json:"snapshots"`
		Stale     bool       `json:"stale"`
		ServerTs  int64      `json:"serverTs"`
	}
	if err := json.Unmarshal(rb, &out); err != nil {
		return nil, fmt.Errorf("parse fetch response: %w", err)
	}
	return out.Snapshots, nil
}

// WaitForCookies returns cookies for a domain. First checks the in-memory
// cache, then long-polls the hub. The `profile` argument is ignored: in pull-
// mode the profile is fixed at registration. Kept for source-compat with
// existing call sites.
func (c *Client) WaitForCookies(ctx context.Context, _profile, domain string, timeout time.Duration) ([]Cookie, error) {
	if cks := c.cachedCookies(domain); len(cks) > 0 {
		return cks, nil
	}
	readyCtx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	if err := c.WaitReady(readyCtx); err != nil {
		return nil, fmt.Errorf("waiting for hub registration: %w", err)
	}

	deadline := time.Now().Add(timeout)
	var lastErr error
	for time.Now().Before(deadline) {
		left := time.Until(deadline)
		pollSec := 8
		if left < 9*time.Second {
			pollSec = int(left/time.Second) + 1
		}
		if pollSec < 1 {
			pollSec = 1
		}
		snaps, err := c.FetchSnapshots(ctx, []string{domain}, pollSec, true)
		if err != nil {
			lastErr = err
		} else if len(snaps) > 0 {
			best := chooseBest(snaps)
			c.cacheStore(domain, best)
			return best.Cookies, nil
		}
		select {
		case <-time.After(2 * time.Second):
		case <-ctx.Done():
			return nil, ctx.Err()
		}
	}
	if lastErr != nil {
		return nil, fmt.Errorf("no cookies for domain=%q within %s (last error: %w)", domain, timeout, lastErr)
	}
	return nil, fmt.Errorf("no cookies for domain=%q within %s", domain, timeout)
}

// GetCookies returns cached cookies if any (no network call).
func (c *Client) GetCookies(_profile, domain string) []Cookie {
	return c.cachedCookies(domain)
}

func (c *Client) cachedCookies(domain string) []Cookie {
	c.mu.RLock()
	defer c.mu.RUnlock()
	e, ok := c.cache[domain]
	if !ok {
		return nil
	}
	if time.Since(e.fetched) > cacheTTL {
		return nil
	}
	return e.snap.Cookies
}

func (c *Client) cacheStore(domain string, s Snapshot) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.cache[domain] = cacheEntry{snap: s, fetched: time.Now()}
}

// chooseBest picks the snapshot with the most cookies; ties broken by newest.
// In multi-account Chrome profiles the hub returns one snapshot per email but
// the cookie blob is shared (Google multi-login), so any non-empty snapshot
// suffices for chromedp injection. Account selection at the URL level
// (/u/<authuserIndex>/) is the orchestration's responsibility, not ours.
func chooseBest(snaps []Snapshot) Snapshot {
	best := snaps[0]
	for _, s := range snaps[1:] {
		if len(s.Cookies) > len(best.Cookies) ||
			(len(s.Cookies) == len(best.Cookies) && s.LastUpdatedAt > best.LastUpdatedAt) {
			best = s
		}
	}
	return best
}

func newNonce() string {
	var b [16]byte
	_, _ = rand.Read(b[:])
	return hex.EncodeToString(b[:])
}

// signRequest mirrors hub/internal/hmac/request.go SignRequest verbatim.
// Canonical: ts + "\n" + nonce + "\n" + METHOD + "\n" + PATH + "\n" + sha256_hex(body)
func signRequest(secret []byte, ts int64, nonce, method, path string, body []byte) string {
	bodyHash := sha256.Sum256(body)
	canon := fmt.Sprintf("%d\n%s\n%s\n%s\n%s",
		ts, nonce, method, path, hex.EncodeToString(bodyHash[:]))
	h := hmac.New(sha256.New, secret)
	h.Write([]byte(canon))
	return "sha256=" + hex.EncodeToString(h.Sum(nil))
}

type secretFile struct {
	ID     string `json:"id"`
	Secret string `json:"secret"`
	HubURL string `json:"hubUrl"`
}

func (c *Client) secretPath() string {
	return filepath.Join(c.cfg.PersistDir, "cb_secret.json")
}

func loadSecret(p string) ([]byte, error) {
	data, err := os.ReadFile(p)
	if err != nil {
		return nil, err
	}
	var f secretFile
	if err := json.Unmarshal(data, &f); err != nil {
		return nil, err
	}
	if f.Secret == "" {
		return nil, fmt.Errorf("empty secret in %s", p)
	}
	return []byte(f.Secret), nil
}

func saveSecret(p string, sec []byte) error {
	if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
		return err
	}
	data, err := json.MarshalIndent(secretFile{Secret: string(sec)}, "", "  ")
	if err != nil {
		return err
	}
	tmp := p + ".tmp"
	if err := os.WriteFile(tmp, data, 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, p)
}

func DefaultPersistDir() string {
	if runtime.GOOS == "windows" {
		if d := os.Getenv("LOCALAPPDATA"); d != "" {
			return filepath.Join(d, "GeminiMCP")
		}
	}
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".config", "gemini-mcp")
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}
