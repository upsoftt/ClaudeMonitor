// probe-oauth: разведка OAuth flow Claude Code CLI с куками claude.ai.
//
// КЛЮЧЕВОЙ ФАКТ (выявлено эмпирически 2026-05-08):
//   /oauth/authorize endpoint живёт на ДОМЕНЕ claude.ai, не на console.anthropic.com.
//   Куки .claude.ai из accounts/<id>.json дают SSO напрямую — Google login не нужен.
//   На странице authorize есть кнопка "Authorize" — кликаем программно.
//
// Запуск:
//   .\probe-oauth.exe [account_id]
//   account_id — id из ClaudeMonitor/accounts/, default acc_5f410b45cf (upsoftt)
//
// .credentials.json НЕ трогается — пишем в probe_result.json.

package main

import (
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
	"path/filepath"
	"strings"
	"time"

	"github.com/playwright-community/playwright-go"
)

const (
	clientID      = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
	callbackPort  = 54545
	scopes        = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"
	authorizeURL  = "https://claude.ai/oauth/authorize"
	tokenEndpoint = "https://console.anthropic.com/v1/oauth/token"
)

var redirectURI = fmt.Sprintf("http://localhost:%d/callback", callbackPort)

// Cookie format saved in ClaudeMonitor accounts/<id>.json (Playwright storage_state).
type savedCookie struct {
	Name     string  `json:"name"`
	Value    string  `json:"value"`
	Domain   string  `json:"domain"`
	Path     string  `json:"path"`
	Expires  float64 `json:"expires"`
	HttpOnly bool    `json:"httpOnly"`
	Secure   bool    `json:"secure"`
	SameSite string  `json:"sameSite"`
}

type storageState struct {
	Cookies []savedCookie     `json:"cookies"`
	Origins []json.RawMessage `json:"origins"`
}

func b64url(b []byte) string {
	return strings.TrimRight(base64.URLEncoding.EncodeToString(b), "=")
}

func genPKCE() (verifier, challenge string) {
	raw := make([]byte, 32)
	_, _ = rand.Read(raw)
	verifier = b64url(raw)
	sum := sha256.Sum256([]byte(verifier))
	challenge = b64url(sum[:])
	return
}

type captured struct {
	Code   string
	State  string
	Error  string
	RawQS  string
	Done   chan struct{}
	closed bool
}

func newCaptured() *captured { return &captured{Done: make(chan struct{})} }

func (c *captured) finish() {
	if !c.closed {
		c.closed = true
		close(c.Done)
	}
}

func runCallbackServer(c *captured) *http.Server {
	mux := http.NewServeMux()
	mux.HandleFunc("/callback", func(w http.ResponseWriter, r *http.Request) {
		c.RawQS = r.URL.RawQuery
		c.Code = r.URL.Query().Get("code")
		c.State = r.URL.Query().Get("state")
		c.Error = r.URL.Query().Get("error")
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		w.WriteHeader(http.StatusOK)
		msg := "Got authorization code!"
		if c.Code == "" {
			msg = "Error: " + c.Error
			if c.Error == "" {
				msg = "No code in callback"
			}
		}
		fmt.Fprintf(w, `<html><body style="font-family:sans-serif;padding:40px"><h2>%s</h2><p>Можно закрыть.</p></body></html>`, msg)
		c.finish()
	})
	srv := &http.Server{Addr: fmt.Sprintf("127.0.0.1:%d", callbackPort), Handler: mux}
	go func() {
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Printf("callback server error: %v", err)
		}
	}()
	return srv
}

func loadStorage(path string) (*storageState, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var s storageState
	if err := json.Unmarshal(raw, &s); err != nil {
		return nil, err
	}
	return &s, nil
}

func injectCookies(ctx playwright.BrowserContext, cks []savedCookie) error {
	pw := make([]playwright.OptionalCookie, 0, len(cks))
	for _, c := range cks {
		path := c.Path
		if path == "" {
			path = "/"
		}
		oc := playwright.OptionalCookie{
			Name:     c.Name,
			Value:    c.Value,
			Domain:   playwright.String(c.Domain),
			Path:     playwright.String(path),
			Secure:   playwright.Bool(c.Secure),
			HttpOnly: playwright.Bool(c.HttpOnly),
		}
		switch strings.ToLower(c.SameSite) {
		case "strict":
			oc.SameSite = playwright.SameSiteAttributeStrict
		case "lax":
			oc.SameSite = playwright.SameSiteAttributeLax
		case "none", "no_restriction":
			oc.SameSite = playwright.SameSiteAttributeNone
		default:
			oc.SameSite = playwright.SameSiteAttributeLax
		}
		if c.Expires > 0 {
			exp := c.Expires
			oc.Expires = &exp
		}
		pw = append(pw, oc)
	}
	return ctx.AddCookies(pw)
}

func exchangeToken(code, verifier string) ([]byte, int, error) {
	form := url.Values{}
	form.Set("grant_type", "authorization_code")
	form.Set("code", code)
	form.Set("code_verifier", verifier)
	form.Set("client_id", clientID)
	form.Set("redirect_uri", redirectURI)
	req, err := http.NewRequest("POST", tokenEndpoint, strings.NewReader(form.Encode()))
	if err != nil {
		return nil, 0, err
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	req.Header.Set("Accept", "application/json")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, 0, err
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(resp.Body)
	return body, resp.StatusCode, err
}

func mask(s string) string {
	if len(s) <= 24 {
		return s
	}
	return s[:18] + "..."
}

func chromiumExecPath() string {
	if p := os.Getenv("LOCALAPPDATA"); p != "" {
		root := filepath.Join(p, "ms-playwright")
		if entries, err := os.ReadDir(root); err == nil {
			best := ""
			for _, e := range entries {
				name := e.Name()
				if !e.IsDir() || !strings.HasPrefix(name, "chromium-") || strings.Contains(name, "headless_shell") {
					continue
				}
				if name > best {
					best = name
				}
			}
			if best != "" {
				for _, sub := range []string{"chrome-win64", "chrome-win"} {
					cand := filepath.Join(root, best, sub, "chrome.exe")
					if _, err := os.Stat(cand); err == nil {
						return cand
					}
				}
			}
		}
	}
	return ""
}

func main() {
	accountID := "acc_5f410b45cf"
	if len(os.Args) > 1 {
		accountID = os.Args[1]
	}

	cwd, _ := os.Getwd()
	projectRoot := filepath.Clean(filepath.Join(cwd, "..", ".."))
	storagePath := filepath.Join(projectRoot, "accounts", accountID+".json")

	fmt.Printf("[*] account_id   : %s\n", accountID)
	fmt.Printf("[*] storage_state: %s\n", storagePath)

	storage, err := loadStorage(storagePath)
	if err != nil {
		log.Fatalf("[!] load storage_state: %v", err)
	}
	domSet := map[string]int{}
	for _, c := range storage.Cookies {
		domSet[c.Domain]++
	}
	fmt.Printf("[*] cookies: %d total, by domain: %v\n", len(storage.Cookies), domSet)

	verifier, challenge := genPKCE()
	stateBytes := make([]byte, 16)
	_, _ = rand.Read(stateBytes)
	stateStr := b64url(stateBytes)
	q := url.Values{}
	q.Set("client_id", clientID)
	q.Set("response_type", "code")
	q.Set("redirect_uri", redirectURI)
	q.Set("scope", scopes)
	q.Set("code_challenge", challenge)
	q.Set("code_challenge_method", "S256")
	q.Set("state", stateStr)
	q.Set("code", "true") // observed in real CLI flow URL — likely "return code, not implicit token"
	full := authorizeURL + "?" + q.Encode()
	fmt.Printf("[*] authorize URL: %s\n", full)

	cap := newCaptured()
	srv := runCallbackServer(cap)
	defer func() {
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		defer cancel()
		_ = srv.Shutdown(ctx)
	}()
	fmt.Printf("[*] callback listening on %s\n", redirectURI)

	pw, err := playwright.Run()
	if err != nil {
		log.Fatalf("[!] playwright.Run: %v", err)
	}
	defer pw.Stop()

	launchOpts := playwright.BrowserTypeLaunchOptions{
		Headless: playwright.Bool(false),
		Args: []string{
			"--disable-blink-features=AutomationControlled",
		},
	}
	if exec := chromiumExecPath(); exec != "" {
		launchOpts.ExecutablePath = playwright.String(exec)
		fmt.Printf("[*] using Chromium: %s\n", exec)
	}

	browser, err := pw.Chromium.Launch(launchOpts)
	if err != nil {
		log.Fatalf("[!] launch: %v", err)
	}
	defer browser.Close()

	bctx, err := browser.NewContext(playwright.BrowserNewContextOptions{
		Viewport:  &playwright.Size{Width: 1280, Height: 900},
		UserAgent: playwright.String("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36"),
		Locale:    playwright.String("en-US"),
	})
	if err != nil {
		log.Fatalf("[!] NewContext: %v", err)
	}
	if err := injectCookies(bctx, storage.Cookies); err != nil {
		log.Fatalf("[!] InjectCookies: %v", err)
	}
	fmt.Printf("[+] injected %d cookies\n", len(storage.Cookies))

	page, err := bctx.NewPage()
	if err != nil {
		log.Fatalf("[!] NewPage: %v", err)
	}
	page.SetDefaultTimeout(15000)
	page.SetDefaultNavigationTimeout(30000)

	// Capture POST request bodies for /v1/oauth/.../authorize via OnRequest.
	page.OnRequest(func(req playwright.Request) {
		u := req.URL()
		if req.Method() == "POST" && strings.Contains(u, "/v1/oauth/") && strings.Contains(u, "/authorize") {
			body, _ := req.PostData()
			fmt.Printf("[req] POST %s\n      body: %s\n", trim(u, 140), trim(body, 300))
		}
	})

	page.OnResponse(func(resp playwright.Response) {
		req := resp.Request()
		u := resp.URL()
		isInteresting := req.ResourceType() == "document" ||
			(req.ResourceType() == "xhr" && strings.Contains(u, "claude.ai/")) ||
			(req.ResourceType() == "fetch" && strings.Contains(u, "claude.ai/"))
		if !isInteresting {
			return
		}
		if !(strings.Contains(u, "claude.ai") || strings.Contains(u, "anthropic.com") || strings.Contains(u, "platform.claude.com")) {
			return
		}
		fmt.Printf("[net] %s %d %s %s\n", req.Method(), resp.Status(), req.ResourceType(), trim(u, 140))

		// On error responses for /v1/oauth/.../authorize, read body in a
		// separate goroutine (CRITICAL: never call resp.Body() inside the
		// callback — it deadlocks the playwright-go pipe reader).
		if resp.Status() >= 400 && strings.Contains(u, "/v1/oauth/") && strings.Contains(u, "/authorize") {
			go func() {
				body, err := resp.Body()
				if err != nil {
					fmt.Printf("[res] body read error: %v\n", err)
					return
				}
				fmt.Printf("[res] HTTP %d body: %s\n", resp.Status(), trim(string(body), 600))
			}()
		}
	})

	// Pre-warm: visit claude.ai first so the server can issue any additional
	// cookies (CSRF, Cloudflare, Segment, etc.) that the OAuth form expects.
	// Our accounts/*.json only stores sessionKey+lastActiveOrg from the
	// extension — the rest comes from server during a real page load.
	fmt.Println("[*] pre-warming session at https://claude.ai/ …")
	if _, err := page.Goto("https://claude.ai/", playwright.PageGotoOptions{
		WaitUntil: playwright.WaitUntilStateNetworkidle,
		Timeout:   playwright.Float(20000),
	}); err != nil {
		fmt.Printf("[!] pre-warm goto: %v (продолжаем)\n", err)
	}
	cks, _ := bctx.Cookies()
	fmt.Printf("[+] cookies after pre-warm: %d\n", len(cks))

	fmt.Println("[*] navigating to authorize URL…")
	if _, err := page.Goto(full, playwright.PageGotoOptions{
		WaitUntil: playwright.WaitUntilStateNetworkidle,
	}); err != nil {
		fmt.Printf("[!] goto error (продолжаем): %v\n", err)
	}
	title, _ := page.Title()
	fmt.Printf("[*] landed at: %s\n", page.URL())
	fmt.Printf("[*] title    : %q\n", title)

	// Try to programmatically click the Authorize button.
	go func() {
		// Wait a moment for the React app to render.
		time.Sleep(2 * time.Second)
		btn := page.GetByRole("button", playwright.PageGetByRoleOptions{
			Name: "Authorize",
		})
		fmt.Println("[*] clicking Authorize button…")
		if err := btn.Click(playwright.LocatorClickOptions{
			Timeout: playwright.Float(20000),
		}); err != nil {
			fmt.Printf("[!] auto-click failed: %v\n", err)
			fmt.Println("    можешь нажать Authorize в окне вручную — probe ждёт callback")
			return
		}
		fmt.Println("[+] Authorize clicked")
	}()

	select {
	case <-cap.Done:
		if cap.Code != "" {
			fmt.Printf("[+] CALLBACK code=%s... state=%s\n", mask(cap.Code), cap.State)
		} else {
			fmt.Printf("[!] CALLBACK error=%q raw=%s\n", cap.Error, cap.RawQS)
		}
	case <-time.After(60 * time.Second):
		title, _ := page.Title()
		fmt.Printf("[!] timeout, no callback\n    final URL: %s\n    title: %q\n", page.URL(), title)

		// Try to read the on-page error message via DOM eval — harmless if no such element.
		bodyText, evalErr := page.Evaluate("() => document.body.innerText.slice(0, 500)")
		if evalErr == nil {
			fmt.Printf("    page body text (first 500 chars): %v\n", bodyText)
		}
	}

	if cap.Code == "" {
		fmt.Println("[*] окно открыто 30 сек для скриншота — посмотри что на экране")
		time.Sleep(30 * time.Second)
	} else {
		time.Sleep(2 * time.Second)
	}

	if cap.Code == "" {
		fmt.Println("[*] нет authorization code — exchange пропущен")
		return
	}

	fmt.Println("[*] exchanging code for tokens…")
	body, status, err := exchangeToken(cap.Code, verifier)
	if err != nil {
		log.Fatalf("[!] exchange: %v", err)
	}
	fmt.Printf("[+] token endpoint HTTP %d\n", status)
	if status != 200 {
		fmt.Printf("    body: %s\n", string(body))
		// Try alternative endpoints if the first fails.
		altEndpoints := []string{
			"https://claude.ai/v1/oauth/token",
			"https://api.anthropic.com/v1/oauth/token",
		}
		for _, ep := range altEndpoints {
			fmt.Printf("[*] trying alt endpoint: %s\n", ep)
			form := url.Values{}
			form.Set("grant_type", "authorization_code")
			form.Set("code", cap.Code)
			form.Set("code_verifier", verifier)
			form.Set("client_id", clientID)
			form.Set("redirect_uri", redirectURI)
			req, _ := http.NewRequest("POST", ep, strings.NewReader(form.Encode()))
			req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
			req.Header.Set("Accept", "application/json")
			resp, err := http.DefaultClient.Do(req)
			if err != nil {
				fmt.Printf("    error: %v\n", err)
				continue
			}
			b, _ := io.ReadAll(resp.Body)
			resp.Body.Close()
			fmt.Printf("    HTTP %d body: %s\n", resp.StatusCode, trim(string(b), 300))
			if resp.StatusCode == 200 {
				body = b
				status = 200
				break
			}
		}
		if status != 200 {
			return
		}
	}

	var payload map[string]any
	_ = json.Unmarshal(body, &payload)
	masked := map[string]any{}
	for k, v := range payload {
		if s, ok := v.(string); ok && len(s) > 24 {
			masked[k] = mask(s)
		} else {
			masked[k] = v
		}
	}
	pretty, _ := json.MarshalIndent(masked, "", "  ")
	fmt.Printf("[+] response (masked):\n%s\n", string(pretty))

	out := filepath.Join(projectRoot, "probe_result.json")
	full_, _ := json.MarshalIndent(payload, "", "  ")
	if err := os.WriteFile(out, full_, 0o644); err != nil {
		fmt.Printf("[!] write probe_result: %v\n", err)
	} else {
		fmt.Printf("[*] saved → %s\n", out)
	}
}

func trim(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}
