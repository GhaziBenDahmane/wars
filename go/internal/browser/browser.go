// Package browser drives headless Chrome for one thing only: the Cloudflare
// Turnstile token that `startRunV2` requires. Turnstile rejects stock headless
// Chrome (error 600010); with the `HeadlessChrome` user agent and the
// automation flag masked it issues a token in a few seconds without any click.
package browser

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/coder/websocket"

	"agentwars/internal/rpc"
)

const (
	TurnstileSiteKey  = "0x4AAAAAAEnsuR9S0axQ8Ifl"
	proxyNoticeMarker = "notify-Notify_"
)

const turnstileJS = `
new Promise((resolve, reject) => {
  const render = () => {
    let box = document.getElementById('agentwars-turnstile');
    if (!box) {
      box = document.createElement('div');
      box.id = 'agentwars-turnstile';
      box.style.cssText = 'position:fixed;top:12px;left:12px;z-index:2147483647';
      document.body.appendChild(box);
    }
    box.innerHTML = '';
    window.turnstile.render(box, {
      sitekey: SITE_KEY,
      callback: (token) => resolve(token),
      'error-callback': (code) => reject(new Error('turnstile error ' + code)),
    });
  };
  setTimeout(() => reject(new Error('turnstile timeout')), TIMEOUT_MS);
  if (window.turnstile) return render();
  const script = document.createElement('script');
  script.src = 'https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit';
  script.onload = render;
  document.head.appendChild(script);
})
`

type Credentials struct {
	TurnstileToken string
	UserAgent      string
	Cookie         string
}

type cdp struct {
	socket *websocket.Conn
	nextID int64
}

func (c *cdp) send(ctx context.Context, method string, params any) (json.RawMessage, error) {
	c.nextID++
	id := c.nextID
	message, err := json.Marshal(map[string]any{"id": id, "method": method, "params": params})
	if err != nil {
		return nil, err
	}
	if err := c.socket.Write(ctx, websocket.MessageText, message); err != nil {
		return nil, err
	}
	for {
		kind, frame, err := c.socket.Read(ctx)
		if err != nil {
			return nil, fmt.Errorf("Chrome closed the DevTools connection: %w", err)
		}
		if kind != websocket.MessageText {
			continue
		}
		var reply struct {
			ID     *int64          `json:"id"`
			Error  json.RawMessage `json:"error"`
			Result json.RawMessage `json:"result"`
		}
		if err := json.Unmarshal(frame, &reply); err != nil {
			return nil, err
		}
		if reply.ID == nil || *reply.ID != id {
			continue // an event
		}
		if len(reply.Error) > 0 {
			return nil, fmt.Errorf("%s: %s", method, reply.Error)
		}
		return reply.Result, nil
	}
}

func (c *cdp) evaluate(ctx context.Context, expression string) (any, error) {
	raw, err := c.send(ctx, "Runtime.evaluate", map[string]any{
		"expression": expression, "awaitPromise": true, "returnByValue": true,
	})
	if err != nil {
		return nil, err
	}
	var result struct {
		Result struct {
			Value any `json:"value"`
		} `json:"result"`
		ExceptionDetails *struct {
			Text      string `json:"text"`
			Exception struct {
				Description *string `json:"description"`
			} `json:"exception"`
		} `json:"exceptionDetails"`
	}
	if err := json.Unmarshal(raw, &result); err != nil {
		return nil, err
	}
	if details := result.ExceptionDetails; details != nil {
		text := details.Text
		if details.Exception.Description != nil {
			text = *details.Exception.Description
		}
		if text == "" {
			text = "evaluation failed"
		}
		return nil, errors.New(text)
	}
	return result.Result.Value, nil
}

// settle waits until the play page has loaded and stopped redirecting.
func (c *cdp) settle(ctx context.Context, settle time.Duration) error {
	started := time.Now()
	var stableSince time.Time
	last := "no answer from the page"
	for time.Since(started) < 30*time.Second {
		value, err := c.evaluate(ctx, "JSON.stringify([location.href, document.readyState])")
		var state []string
		if text, ok := value.(string); err == nil && ok && json.Unmarshal([]byte(text), &state) == nil && len(state) == 2 {
			href, ready := state[0], state[1]
			last = href + " " + ready
			if strings.Contains(href, proxyNoticeMarker) {
				return errors.New("a proxy disclaimer page is showing instead of the site")
			}
			if ready == "complete" && strings.Contains(href, "/play/") {
				if stableSince.IsZero() {
					stableSince = time.Now()
				}
				if time.Since(stableSince) >= settle {
					return nil
				}
			} else {
				stableSince = time.Time{}
			}
		}
		time.Sleep(200 * time.Millisecond)
	}
	return fmt.Errorf("the play page did not finish loading (last: %s)", last)
}

// localClient reaches Chrome's DevTools HTTP endpoints, never through a proxy.
var localClient = &http.Client{
	Transport: &http.Transport{Proxy: nil},
	Timeout:   3 * time.Second,
}

func getJSON(ctx context.Context, url string, target any) error {
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return err
	}
	response, err := localClient.Do(request)
	if err != nil {
		return err
	}
	defer response.Body.Close()
	return json.NewDecoder(response.Body).Decode(target)
}

func devtoolsReady(ctx context.Context, cdpURL string, exited <-chan error) (map[string]any, error) {
	for range 80 {
		select {
		case err := <-exited:
			return nil, fmt.Errorf("Chrome exited early with %v", err)
		default:
		}
		var version map[string]any
		if getJSON(ctx, cdpURL+"/json/version", &version) == nil {
			return version, nil
		}
		time.Sleep(250 * time.Millisecond)
	}
	return nil, fmt.Errorf("Chrome did not open its DevTools port at %s", cdpURL)
}

// userAgent is the user agent of this Chrome without "Headless". It has to be
// a command line flag: a CDP override does not reach Turnstile's cross-origin
// iframe.
func userAgent(ctx context.Context, chrome string) (string, error) {
	output, err := exec.CommandContext(ctx, chrome, "--version").Output()
	if err != nil {
		return "", fmt.Errorf("cannot start Chrome at %q: %w", chrome, err)
	}
	for _, word := range strings.Fields(string(output)) {
		major, _, _ := strings.Cut(word, ".")
		if _, err := strconv.ParseUint(major, 10, 32); err == nil {
			return "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/" +
				major + ".0.0.0 Safari/537.36", nil
		}
	}
	return "", fmt.Errorf("unexpected `%s --version` output: %s", chrome, output)
}

func launch(chrome string, port int, profile, userAgent string) (*exec.Cmd, error) {
	command := exec.Command(chrome,
		"--user-agent="+userAgent,
		"--headless=new",
		"--remote-debugging-port="+strconv.Itoa(port),
		"--user-data-dir="+profile,
		"--no-sandbox", // containers rarely allow Chrome's own sandbox
		"--disable-dev-shm-usage",
		"--disable-gpu",
		"--no-first-run",
		"--no-default-browser-check",
		"--window-size=1280,900",
		"--disable-blink-features=AutomationControlled",
		"about:blank",
	)
	if err := command.Start(); err != nil {
		return nil, fmt.Errorf("cannot start Chrome at %q: %w", chrome, err)
	}
	return command, nil
}

// freePort is a port nothing listens on, so racers running side by side each
// get their own Chrome.
func freePort() (int, error) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return 0, err
	}
	defer listener.Close()
	return listener.Addr().(*net.TCPAddr).Port, nil
}

// Get launches (or attaches to `cdpURL`) Chrome, opens the play page and gets
// a token.
func Get(ctx context.Context, chrome, cdpURL, profile, playURL string, timeout time.Duration) (*Credentials, error) {
	port, err := freePort()
	if err != nil {
		return nil, err
	}
	// A throwaway profile unless one is given (kept, e.g. with accepted notices).
	temporary := profile == ""
	if temporary {
		profile = filepath.Join(os.TempDir(), fmt.Sprintf("agentwars-chrome-%d", os.Getpid()))
	}
	var launchedAgent string
	var exited chan error
	if cdpURL == "" {
		if launchedAgent, err = userAgent(ctx, chrome); err != nil {
			return nil, err
		}
		child, err := launch(chrome, port, profile, launchedAgent)
		if err != nil {
			return nil, err
		}
		exited = make(chan error, 1)
		go func() { exited <- child.Wait() }()
		defer func() {
			child.Process.Kill() // free the CPU before the race starts
			<-exited
			if temporary {
				os.RemoveAll(profile)
			}
		}()
		cdpURL = fmt.Sprintf("http://127.0.0.1:%d", port)
	}
	version, err := devtoolsReady(ctx, cdpURL, exited)
	if err != nil {
		return nil, err
	}
	agent := launchedAgent
	if agent == "" {
		raw, _ := version["User-Agent"].(string)
		agent = strings.ReplaceAll(raw, "HeadlessChrome", "Chrome")
	}
	var targets []struct {
		Type                 string `json:"type"`
		WebSocketDebuggerURL string `json:"webSocketDebuggerUrl"`
	}
	if err := getJSON(ctx, cdpURL+"/json/list", &targets); err != nil {
		return nil, err
	}
	wsURL := ""
	found := false
	for _, target := range targets {
		if target.Type == "page" {
			wsURL, found = target.WebSocketDebuggerURL, true
			break
		}
	}
	if !found {
		return nil, errors.New("Chrome has no page target")
	}
	if wsURL == "" {
		return nil, errors.New("no DevTools URL")
	}
	socket, _, err := websocket.Dial(ctx, wsURL, &websocket.DialOptions{HTTPClient: localClient})
	if err != nil {
		return nil, fmt.Errorf("DevTools websocket: %w", err)
	}
	socket.SetReadLimit(64 << 20)
	defer socket.CloseNow()
	c := &cdp{socket: socket}
	if _, err := c.send(ctx, "Network.enable", map[string]any{}); err != nil {
		return nil, err
	}
	if _, err := c.send(ctx, "Page.navigate", map[string]any{"url": playURL}); err != nil {
		return nil, err
	}
	if err := c.settle(ctx, 1500*time.Millisecond); err != nil {
		return nil, err
	}
	siteKey, _ := json.Marshal(TurnstileSiteKey)
	script := strings.NewReplacer("SITE_KEY", string(siteKey),
		"TIMEOUT_MS", strconv.FormatInt(timeout.Milliseconds(), 10)).Replace(turnstileJS)
	token, err := c.evaluate(ctx, script)
	if err != nil {
		return nil, fmt.Errorf("Turnstile: %w", err)
	}
	raw, err := c.send(ctx, "Network.getCookies", map[string]any{"urls": []string{rpc.Origin + "/"}})
	if err != nil {
		return nil, err
	}
	var cookies struct {
		Cookies []struct {
			Name  string `json:"name"`
			Value string `json:"value"`
		} `json:"cookies"`
	}
	json.Unmarshal(raw, &cookies)
	pairs := make([]string, len(cookies.Cookies))
	for i, cookie := range cookies.Cookies {
		pairs[i] = cookie.Name + "=" + cookie.Value
	}
	text, ok := token.(string)
	if !ok {
		return nil, errors.New("empty Turnstile token")
	}
	return &Credentials{TurnstileToken: text, UserAgent: agent, Cookie: strings.Join(pairs, "; ")}, nil
}
