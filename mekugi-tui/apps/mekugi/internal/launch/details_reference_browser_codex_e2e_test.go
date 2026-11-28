//go:build codex_e2e

package launch

import (
	"context"
	"fmt"
	"image"
	"image/png"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
	"time"

	"github.com/charmbracelet/x/vt"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/connector"
	"github.com/mekugi-harness/mekugi/mekugi-webui"
)

func TestDetailLinksAndReferenceThroughNativeBrowser(t *testing.T) {
	if os.Getenv("MEKUGI_TEST_WEBUI_BROWSER") != "1" {
		t.Skip("set MEKUGI_TEST_WEBUI_BROWSER=1 after just webui")
	}
	t.Setenv("AGENT_BROWSER_ENGINE", "chrome")
	bin := browserFixtureCommand(t)
	ctx, cancel := context.WithTimeout(t.Context(), 90*time.Second)
	defer cancel()
	id, err := exec.CommandContext(ctx, bin, "session", "id", "--scope", "worktree", "--prefix", "details-reference").Output()
	if err != nil {
		t.Fatal(err)
	}
	session := strings.TrimSpace(string(id))
	defer func() {
		cleanup, stop := context.WithTimeout(context.Background(), 10*time.Second)
		defer stop()
		_ = exec.CommandContext(cleanup, bin, "--session", session, "close").Run()
	}()
	run := func(args ...string) string {
		t.Helper()
		output, err := exec.CommandContext(ctx, bin, append([]string{"--session", session}, args...)...).CombinedOutput()
		if err != nil {
			snap, _ := exec.CommandContext(ctx, bin, "--session", session, "snapshot").CombinedOutput()
			t.Fatalf("browser %v: %v: %s\n%s", args, err, output, snap)
		}
		return strings.TrimSpace(string(output))
	}
	page := httptest.NewServer(webui.Handler())
	defer page.Close()
	const token = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
	tokenFile := filepath.Join(shortTempDir(t), "token")
	if err := os.WriteFile(tokenFile, []byte(token+"\n"), 0600); err != nil {
		t.Fatal(err)
	}
	workspace := t.TempDir()
	if err := os.WriteFile(filepath.Join(workspace, "notes.md"), []byte("first\nfile range line\nthird\n"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(workspace, "not-an-image.png"), []byte("still text\n"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(workspace, ".ignore"), []byte("not-an-image.png\n"), 0600); err != nil {
		t.Fatal(err)
	}
	skill := filepath.Join(workspace, ".agents", "skills", "fixture-helper")
	if err := os.MkdirAll(filepath.Join(skill, "agents"), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(skill, "SKILL.md"), []byte("---\nname: fixture-helper\ndescription: A browser fixture skill\n---\n\nFixture instructions sentinel.\n"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(skill, "agents", "openai.yaml"), []byte("interface:\n  display_name: Fixture helper\n"), 0600); err != nil {
		t.Fatal(err)
	}
	file, err := os.Create(filepath.Join(workspace, "image-without-extension"))
	if err != nil {
		t.Fatal(err)
	}
	err = png.Encode(file, image.NewRGBA(image.Rect(0, 0, 2, 2)))
	closeErr := file.Close()
	if err != nil || closeErr != nil {
		t.Fatalf("write fixture image: %v; close: %v", err, closeErr)
	}
	imageUpload := filepath.Join(t.TempDir(), "browser-image.png")
	pixels, err := os.ReadFile(file.Name())
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(imageUpload, pixels, 0600); err != nil {
		t.Fatal(err)
	}
	connect := make(chan string, 1)
	integration := appServerIntegration{controlSocket: connect}
	const uploadContent = "UTF-8 upload Ω sentinel."
	upload := filepath.Join(t.TempDir(), "browser-upload.txt")
	if err := os.WriteFile(upload, []byte(uploadContent), 0600); err != nil {
		t.Fatal(err)
	}
	quoted := make(chan struct{}, 1)
	created := make(chan struct{}, 1)
	oneOffThread := make(chan string, 1)
	const oneOffTask = "Check the One-off file link."
	provider := &toolFrontendCodexProvider{
		program: `const r = await tools.exec_command({cmd:"for i in $(seq 1 160); do printf 'native run line %s\\n' \"$i\"; done; cat notes.md",shell:"/bin/bash",login:false,yield_time_ms:10000});
text(r.event_call_id); text(r.output);
await tools.exec_command({cmd:"sleep 81",shell:"/bin/bash",login:false,yield_time_ms:100});
await tools.exec_command({cmd:"sleep 82",shell:"/bin/bash",login:false,yield_time_ms:100});`,
		expected:   []string{"native run line 160"},
		prediction: "Could you explain the workspace notes?",
	}
	runID := regexp.MustCompile(`exec-[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}`)
	provider.observeRequest = func(body []byte, headers http.Header) error {
		if id := runID.Find(body); len(id) > 0 && !strings.Contains(provider.finalText, "Workspace notes") {
			provider.finalText = fmt.Sprintf("Reference me. [Workspace notes](notes.md#L2) [Run output](mekugi:run/%s#L130-L132) [Unavailable output](mekugi:run/%s#L900)", id, id)
		}
		if wire := string(body); strings.Contains(wire, "Check references") &&
			strings.Contains(wire, "User selections:") && strings.Contains(wire, "file range line") &&
			strings.Contains(wire, "Reference me.") && strings.Contains(wire, "Fixture instructions sentinel.") &&
			strings.Contains(wire, "browser-upload.txt") && strings.Contains(wire, uploadContent) {
			select {
			case quoted <- struct{}{}:
			default:
			}
		}
		if strings.Contains(string(body), "First browser message.") {
			select {
			case created <- struct{}{}:
			default:
			}
		}
		if strings.Contains(string(body), oneOffTask) {
			metadata, _ := fixtureTurnMetadata(headers)
			select {
			case oneOffThread <- metadata.ThreadID:
			default:
			}
		}
		return nil
	}
	run("open", page.URL)
	run("set", "viewport", "1280", "900")
	run("find", "label", "Machine name", "fill", "Details fixture")
	run("find", "label", "Access token", "fill", token)
	runAppServerPreviewWith(t, provider, nil, appServerPreview{
		managedRuntime: true, integration: &integration, workspace: workspace,
		completedText: "Reference me.", saveBeforePrompt: true,
		environment: []string{"SHELL=/bin/bash", "CODEX_HOME=" + t.TempDir()},
		beforePrompt: func(t *testing.T, _ io.Writer, _ func(string), _ func(func(string) bool), _ *vt.Emulator) {
			bridge, err := connector.New(connector.Options{SocketPath: <-connect, TokenFile: tokenFile, Origins: []string{page.URL}})
			if err != nil {
				t.Fatal(err)
			}
			t.Cleanup(func() { _ = bridge.Close() })
			remote := httptest.NewServer(bridge)
			t.Cleanup(remote.Close)
			run("find", "label", "Connector URL", "fill", remote.URL)
			run("find", "role", "button", "click", "--name", "Connect", "--exact")
			run("wait", "nav[aria-label=Threads] li a")
			run("click", "nav[aria-label=Threads] li a")
			run("wait", "#composer-text")
		},
		afterPrompt: func(t *testing.T, _ io.Writer, _ func(string), _ func(func(string) bool), _ *vt.Emulator) {
			run("wait", "--text", "Workspace notes")
			run("wait", "--fn", `[...document.querySelectorAll('[data-entry]')].some(entry =>
				!entry.textContent.includes('native run line 160') &&
				[...entry.querySelectorAll('[data-ai-element="tool"]')].some(row =>
					row.querySelector('[data-slot="trace-title"]')?.textContent.trim() === 'Read' &&
					row.querySelector('code')?.textContent.trim() === 'notes.md'))`)
			setBrowserViewport(t, run, 360, 800)
			run("find", "role", "button", "click", "--name", "Session details", "--exact")
			run("wait", "--fn", fmt.Sprintf(`(() => {
				const panel = document.querySelector('[aria-label="Session details"]:not(button)');
				const path = [...(panel?.querySelectorAll('span[title]') ?? [])].find(node => node.title === %q);
				if (!path || path.textContent !== path.title) return false;
				const box = panel.getBoundingClientRect();
				const value = path.getBoundingClientRect();
				return box.left >= 0 && box.right <= innerWidth && value.width > 0 &&
					value.left >= box.left && value.right <= box.right &&
					panel.scrollWidth <= panel.clientWidth + 1;
			})()`, workspace))
			run("wait", "--fn", `(() => {
				const labels = [...document.querySelectorAll('[aria-label="Session details"] dt')];
				const count = (label) => labels.find(node => node.textContent === label)
					?.nextElementSibling?.textContent;
				const input = count("Input tokens");
				return input === "20+" && count("Cached input tokens") === "0+" &&
					count("Uncached input tokens") === "20+";
			})()`)
			run("press", "Escape")
			run("wait", "--fn", `document.querySelector('button[aria-label="Session details"]')?.getAttribute('aria-expanded') === 'false'`)
			setBrowserViewport(t, run, 1280, 900)
			run("wait", `[aria-label="Active processes"] button[aria-label="Stop sleep 81"]`)
			run("wait", `[aria-label="Active processes"] button[aria-label="Stop sleep 82"]`)
			checkComposerPresets(t, run)
			run("fill", "#composer-text", "/ps")
			run("press", "Enter")
			run("wait", "--text", "Active processes appear above the composer.")
			run("find", "role", "button", "click", "--name", "Stop sleep 81", "--exact")
			run("wait", "--fn", `!document.querySelector('[aria-label="Stop sleep 81"]') && !!document.querySelector('[aria-label="Stop sleep 82"]')`)
			run("find", "role", "button", "click", "--name", "Stop sleep 82", "--exact")
			run("wait", "--fn", `!document.querySelector('[aria-label="Active processes"]')`)
			run("fill", "#composer-text", "/oneoff "+oneOffTask)
			run("press", "Enter")
			run("wait", "--fn", `document.querySelector('[role=tab][aria-selected=true]')?.textContent.trim() === 'One-off'`)
			var scopedThread string
			select {
			case scopedThread = <-oneOffThread:
				if scopedThread == "" {
					t.Fatal("the One-off provider request had no thread identity")
				}
			case <-ctx.Done():
				t.Fatal("the One-off provider request did not arrive")
			}
			run("wait", "--text", "Workspace notes")
			run("eval", `(() => {
				const fetch = window.fetch;
				window.fetch = (...args) => {
					if (String(args[0]).endsWith('/api/resources/file')) {
						document.body.dataset.checkedFileThread = JSON.parse(args[1].body).thread;
						window.fetch = fetch;
					}
					return fetch(...args);
				};
			})()`)
			run("find", "role", "link", "click", "--name", "Workspace notes", "--exact")
			run("wait", "--fn", fmt.Sprintf(`document.body.dataset.checkedFileThread === %q &&
				document.querySelector('#activity-details mark')?.textContent?.trim() === 'file range line'`, scopedThread))
			run("find", "role", "button", "click", "--name", "Close details", "--exact")
			run("find", "role", "button", "click", "--name", "Close One-off", "--exact")
			run("wait", "--fn", `!document.querySelector('[role=tablist][aria-label=Conversation]')`)
			run("fill", "#composer-text", "Keep typing")
			run("eval", `(() => {
				const clipboardData = new DataTransfer();
				clipboardData.items.add(new File(["note"], "pasted.txt", { type: "text/plain" }));
				document.querySelector('#composer-text').dispatchEvent(
					new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData }),
				);
			})()`)
			run("wait", "--fn", `document.activeElement?.id === 'composer-text' &&
				document.activeElement.querySelector('[data-draft-attachment]')?.textContent === '[File pasted.txt]'`)
			run("find", "role", "button", "click", "--name", "Remove pasted.txt", "--exact")
			run("fill", "#composer-text", "@!not-an-image")
			run("wait", "[aria-label=Files] [role=option]")
			run("find", "role", "option", "click", "--name", "not-an-image.png", "--exact")
			run("wait", "--fn", `document.querySelector('#composer-text')?.textContent === "@'not-an-image.png' " && document.querySelector('#composer-text [data-draft-attachment]')?.classList.contains('text-positive') && document.querySelector('[aria-label="Attachments"] li')?.textContent.includes('not-an-image.png') && !document.querySelector('[aria-label="Attachments"] img')`)
			run("find", "role", "button", "click", "--name", "Remove not-an-image.png", "--exact")
			const beforeImage = "Some words before [Image literal.png] "
			run("fill", "#composer-text", beforeImage)
			run("find", "role", "button", "click", "--name", "Attach", "--exact")
			run("find", "role", "menuitem", "click", "--name", "Attach path on Details fixture", "--exact")
			run("fill", "#attach-path", "image-without-extension")
			run("find", "role", "button", "click", "--name", "Attach path", "--exact")
			const imagePlaceholder = "[Image image-without-extension]"
			checkDraftImageToken(run, "#composer-text", beforeImage, imagePlaceholder)
			run("wait", "--fn", `document.querySelector('[aria-label="Attachments"] img')?.naturalWidth === 2`)
			setBrowserViewport(t, run, 390, 844)
			run("wait", "--fn", `(() => {
				const field = document.querySelector('#composer-text');
				const image = field?.querySelector('[data-draft-attachment]');
				if (!image) return false;
				const start = new Range();
				start.setStart(field.firstChild, 0);
				start.setEnd(field.firstChild, 1);
				const label = new Range();
				label.selectNodeContents(image);
				return label.getClientRects().length === 1 &&
					image.getBoundingClientRect().top > start.getBoundingClientRect().top + 10 &&
					image.getBoundingClientRect().right <= field.getBoundingClientRect().right;
			})()`)
			run("eval", `(() => {
				const field = document.querySelector('#composer-text');
				field.focus();
				document.getSelection().collapse(field.lastChild, 0);
			})()`)
			run("press", "Shift+ArrowLeft")
			run("wait", "--fn", fmt.Sprintf(`document.getSelection().toString() === %q`, imagePlaceholder))
			run("press", "Backspace")
			run("wait", "--fn", fmt.Sprintf(`document.querySelector('#composer-text').textContent === %q &&
				!document.querySelector('[aria-label="Attachments"]')`, beforeImage+" "))
			run("press", "Control+z")
			checkDraftImageToken(run, "#composer-text", beforeImage, imagePlaceholder)
			setBrowserViewport(t, run, 1280, 900)
			run("find", "role", "button", "click", "--name", "Remove image-without-extension", "--exact")
			run("fill", "#composer-text", "")
			run("find", "role", "link", "click", "--name", "Workspace notes", "--exact")
			run("wait", "--fn", `document.querySelector('#activity-details mark')?.textContent?.trim() === 'file range line'`)
			run("eval", `(() => { const r = new Range(); r.selectNodeContents(document.querySelector('#activity-details mark')); document.getSelection().removeAllRanges(); document.getSelection().addRange(r); })()`)
			run("wait", "--fn", `[...document.querySelectorAll('#activity-details button')].some(button => button.textContent.trim() === 'Reference')`)
			run("find", "role", "button", "click", "--name", "Reference", "--exact")
			run("wait", "--fn", `document.querySelector('#composer-text').textContent === '[Selected dialog] ' && !document.querySelector('[aria-label="Attachments"]')`)
			run("find", "role", "button", "click", "--name", "Close details", "--exact")
			run("wait", "#activity-details", "--state", "hidden")
			run("eval", `(() => { const text = [...document.querySelectorAll('[data-entry] .prose p')].find(p => p.textContent.startsWith('Reference me.')).firstChild; const r = new Range(); r.setStart(text, 0); r.setEnd(text, 'Reference me.'.length); document.getSelection().removeAllRanges(); document.getSelection().addRange(r); })()`)
			run("wait", "--fn", `[...document.querySelectorAll('button')].some(button => button.textContent.trim() === 'Reference')`)
			run("find", "role", "button", "click", "--name", "Reference", "--exact")
			run("wait", "--fn", `document.querySelector('#composer-text').textContent === '[Selected dialog] [Selected message] ' && !document.querySelector('[aria-label="Attachments"]')`)
			run("find", "role", "link", "click", "--name", "Run output", "--exact")
			run("wait", "--fn", `document.querySelector('#activity-details')?.textContent.includes('Batch')`)
			run("wait", "--fn", `(() => { const mark = document.querySelector('#activity-details mark'); if (mark?.textContent !== 'native run line 130\nnative run line 131\nnative run line 132') return false; const r = mark.getBoundingClientRect(); const pane = document.querySelector('#activity-details > .overflow-y-auto'); return pane.scrollTop > 0 && r.top >= pane.getBoundingClientRect().top && r.bottom <= pane.getBoundingClientRect().bottom; })()`)
			run("find", "role", "button", "click", "--name", "Close details", "--exact")
			run("find", "role", "link", "click", "--name", "Unavailable output", "--exact")
			run("wait", "--text", "The referenced output lines are unavailable.")
			run("find", "role", "button", "click", "--name", "Close details", "--exact")
			run("fill", "#composer-text", "[Selected dialog] [Selected message] Check references $fixture")
			run("wait", `[aria-label="Skills and presets"] [role=option]`)
			run("find", "role", "option", "click", "--name", "Fixture helper")
			run("wait", "--fn", `document.querySelector('#composer-text').textContent.endsWith('$fixture-helper ')`)
			run("upload", `input[aria-label="Upload files"]`, upload)
			run("wait", "--fn", `document.querySelector('#composer-text').textContent.endsWith('[File browser-upload.txt] ') && document.querySelector('[aria-label="Attachments"] li')?.textContent.includes('browser-upload.txt')`)
			run("press", "Enter")
			run("wait", "--fn", `Array.from(document.querySelectorAll('[data-entry]')).some(row => row.textContent.includes('Check references') && row.textContent.includes('[File browser-upload.txt]') && !row.textContent.includes('User selections:') && !row.textContent.includes('UTF-8 upload Ω sentinel.'))`)
			select {
			case <-quoted:
			case <-ctx.Done():
				t.Fatal("provider did not receive both references, the selected skill, and the text upload")
			}
			shellOutput := filepath.Join(workspace, "queued-shell.txt")
			run("fill", "#composer-text", "!printf queued > '"+strings.ReplaceAll(shellOutput, "'", "'\\''")+"'")
			run("press", "Tab")
			timer := time.NewTicker(25 * time.Millisecond)
			defer timer.Stop()
			for {
				if contents, _ := os.ReadFile(shellOutput); string(contents) == "queued" {
					break
				}
				select {
				case <-timer.C:
				case <-ctx.Done():
					t.Fatal("Tab did not run the native queued shell draft")
				}
			}
			run("find", "role", "button", "click", "--name", "New thread", "--exact")
			run("wait", "--fn", fmt.Sprintf(`document.querySelector('button[title=%s]') !== null`, fmt.Sprintf("%q", workspace)))
			run("click", fmt.Sprintf("button[title=%q]", workspace))
			run("wait", "--fn", fmt.Sprintf(`document.querySelector('input[aria-label="Workspace path"]')?.value === %q`, workspace))
			manualWorkspace := t.TempDir()
			run("find", "label", "Workspace path", "fill", manualWorkspace)
			run("press", "Enter")
			run("wait", fmt.Sprintf("button[title=%q]", manualWorkspace))
			run("fill", "#new-text", "First browser message.")
			run("upload", `input[type="file"]`, imageUpload)
			checkDraftImageToken(run, "#new-text", "First browser message. ", "[Image browser-image.png]")
			run("find", "role", "button", "click", "--name", "Start thread", "--exact")
			select {
			case <-created:
			case <-time.After(8 * time.Second):
				t.Fatal("the browser's new thread did not submit its first message")
			}
			run("wait", "--fn", `[...document.querySelectorAll('[data-entry]')].some(row => row.textContent.includes('First browser message.'))`)
			run("wait", "--fn", `[...document.querySelectorAll('[data-kind=user]')].some(row => row.textContent.includes('[Image browser-image.png]') && row.querySelector('img')?.naturalWidth === 2)`)
			release := filepath.Join(t.TempDir(), "release-shell")
			for _, test := range []struct {
				message, path, action string
			}{
				{"Keep this browser edit", "keep.txt", "Keep edits"},
				{"Undo this browser edit", "undo.txt", "Revert edits"},
			} {
				idle := `!!document.querySelector('#composer-text:enabled') && !document.querySelector('button[aria-label="Interrupt turn"]')`
				run("wait", "--fn", idle)
				run("fill", "#composer-text", test.message)
				run("press", "Enter")
				run("wait", "--text", test.message)
				run("wait", "--fn", idle)
				path := filepath.Join(manualWorkspace, test.path)
				cmd := "!printf captured > '" + strings.ReplaceAll(path, "'", "'\\''") + "'"
				if test.action == "Keep edits" {
					// Don't finish until Chrome has painted the diff; timeout also bounds a failed fixture.
					cmd += " && for ((i=0; i<300; i++)); do if [ -f '" +
						strings.ReplaceAll(release, "'", "'\\''") +
						"' ]; then exit 0; fi; sleep 0.1; done; exit 1"
				}
				if test.action == "Keep edits" {
					run("fill", "#composer-text", "!")
					run("wait", "--fn", `document.querySelector('#composer-text')?.value === '!' &&
						document.getElementById('composer-shell-target')?.textContent.includes('Shell command on Details fixture')`)
				}
				run("fill", "#composer-text", cmd)
				if test.action == "Keep edits" {
					run("eval", `(() => { const field = document.querySelector('#composer-text');
						field.setSelectionRange(1, 7); field.dispatchEvent(new Event('select')); })()`)
					run("wait", "--fn", `(() => { const field = document.querySelector('#composer-text');
						return field.selectionStart === 1 && field.selectionEnd === 7 &&
							!field.classList.contains('shell-highlighted'); })()`)
				}
				run("press", "Enter")
				for {
					if contents, _ := os.ReadFile(path); string(contents) == "captured" {
						break
					}
					select {
					case <-timer.C:
					case <-ctx.Done():
						t.Fatal("browser shell did not write its rewind fixture file")
					}
				}
				if test.action == "Keep edits" {
					run("wait", `[aria-label="Live edits"]`, "--state", "hidden")
					if err := os.WriteFile(release, nil, 0600); err != nil {
						t.Fatal(err)
					}
					run("find", "role", "button", "click", "--name", "Changes", "--exact")
					run("wait", "--fn", `(() => {
						const panel = [...document.querySelectorAll('aside')].find(pane => pane.textContent.includes('Saved Diff'));
						const file = [...(panel?.querySelectorAll('[data-file]') ?? [])].find(
							entry => entry.querySelector('summary')?.textContent.includes('keep.txt'));
						return !document.querySelector('[aria-label="Live edits"]') &&
							file?.textContent.includes('User shell') &&
							file.querySelector('.bg-add')?.textContent.includes('captured');
					})()`)
					run("find", "role", "button", "click", "--name", "Changes", "--exact")
				}
				run("wait", "--fn", idle)
				run("fill", "#composer-text", "/rewind")
				run("press", "Enter")
				waitForChoice := func(label string) {
					run("wait", "--fn", fmt.Sprintf(
						`[...document.querySelectorAll('[role=dialog] button:enabled')].some(b => b.textContent.trim() === %q)`,
						label))
				}
				waitForChoice(test.message)
				run("find", "role", "button", "click", "--name", test.message, "--exact")
				// The captured-edit preview adds Revert edits and moves Keep edits.
				// Wait for both choices before targeting either button.
				waitForChoice("Revert edits")
				run("find", "role", "button", "click", "--name", test.action, "--exact")
				run("wait", "--fn", fmt.Sprintf(`!document.querySelector('[role=dialog]') && ![...document.querySelectorAll('[data-kind=user]')].some(row => row.textContent.includes(%q))`, test.message))
				if test.action == "Keep edits" {
					if contents, err := os.ReadFile(path); err != nil || string(contents) != "captured" {
						t.Fatalf("Keep edits changed the workspace file: %q, %v", contents, err)
					}
				} else if _, err := os.Stat(path); !os.IsNotExist(err) {
					t.Fatalf("Revert edits did not remove the captured file: %v", err)
				}
			}
			run("wait", "--text", "Reference me.")
			run("find", "role", "button", "click", "--name", "Display settings")
			run("click", `[aria-label="Predictions"] button[data-value="on"]`)
			run("press", "Escape")
			run("wait", "--text", provider.prediction)
			run("focus", "#composer-text")
			run("press", "Tab")
			run("wait", "--fn", fmt.Sprintf(`document.activeElement?.id === 'composer-text' &&
				document.activeElement.value === %q && !document.querySelector('button[aria-label="Interrupt turn"]')`, provider.prediction))
			run("fill", "#composer-text", "")
			run("find", "role", "button", "click", "--name", "Display settings")
			run("click", `[aria-label="Predictions"] button[data-value="off"]`)
			run("click", `[aria-label="Predictions"] button[data-value="on"]`)
			run("press", "Escape")
			run("wait", "--text", provider.prediction)
			run("find", "role", "button", "click", "--name", provider.prediction, "--exact")
			run("wait", "--fn", fmt.Sprintf(`document.activeElement?.id === 'composer-text' &&
				document.activeElement.value === %q`, provider.prediction))
			run("fill", "#composer-text", "")
			run("find", "role", "button", "click", "--name", "Display settings")
			run("click", `[aria-label="Predictions"] button[data-value="off"]`)
			run("press", "Escape")
			run("wait", "--fn", `!document.getElementById('composer-prediction')`)
		},
	})
}

func checkDraftImageToken(run func(...string) string, selector, prefix, placeholder string) {
	run("wait", "--fn", fmt.Sprintf(`(() => {
		const field = document.querySelector(%q);
		const image = field?.querySelector('[data-draft-attachment]');
		return image?.textContent === %q && field.querySelectorAll('[data-draft-attachment]').length === 1 &&
			field.firstChild.nodeType === Node.TEXT_NODE && field.firstChild.data === %q &&
			image.classList.contains('text-marker-image') &&
			getComputedStyle(image).fontWeight === '600' && !image.isContentEditable &&
			field.textContent === %q;
	})()`, selector, placeholder, prefix, prefix+placeholder+" "))
}

func setBrowserViewport(t *testing.T, run func(...string) string, width, height int) {
	t.Helper()
	run("set", "viewport", fmt.Sprintf("%d", width), fmt.Sprintf("%d", height))
	run("wait", "--fn", fmt.Sprintf(`(() => {
		if (innerWidth !== %d || innerHeight !== %d) return false;
		const wide = %t;
		return Boolean(document.querySelector('nav[aria-label="Threads"]')) === wide &&
			Boolean(document.querySelector('button[aria-label="Show sidebar"]')) === !wide;
	})()`, width, height, width >= 900))
}

func checkComposerPresets(t *testing.T, run func(...string) string) {
	t.Helper()
	button := func(name string) {
		run("find", "role", "button", "click", "--name", name, "--exact")
	}
	open := func() {
		run("wait", "--fn", `(() => {
			const b = document.querySelector('button[aria-label="Presets"]');
			if (!b || b.disabled) return false;
			const r = b.getBoundingClientRect();
			return r.width > 0 && r.height > 0 && r.left >= 0 && r.right <= innerWidth && r.top >= 0 && r.bottom <= innerHeight;
		})()`)
		button("Presets")
		run("wait", `[role=dialog] li:first-child button:enabled`)
	}
	for _, width := range []int{1280, 768, 390} {
		setBrowserViewport(t, run, width, 844)
		if width == 390 {
			run("set", "media", "dark")
			run("wait", "--fn", `matchMedia('(prefers-color-scheme: dark)').matches`)
		}
		open()
		run("wait", "--fn", `(() => {
			const dialog = document.querySelector('[role=dialog]');
			const row = dialog.querySelector('li button');
			const box = dialog.getBoundingClientRect();
			return dialog.querySelectorAll('ul kbd').length === 10 &&
				box.left >= 0 && box.right <= innerWidth && dialog.scrollWidth <= dialog.clientWidth &&
				(innerWidth > 640 || row.getBoundingClientRect().height >= 44);
		})()`)
		run("press", "Escape")
		run("wait", "--fn", `!document.querySelector('[role=dialog]')`)
	}

	run("fill", "#composer-text", "Review carefully.\nKeep this line.")
	open()
	run("press", "1")
	run("wait", "--fn", `document.querySelector('[role=dialog] textarea')?.value === 'Review carefully.\nKeep this line.'`)
	run("find", "label", "Label", "fill", "你好")
	run("find", "label", "Prompt or slash command", "fill", "!echo not a preset")
	button("Save")
	run("wait", "--text", "Presets support prompts and slash commands, not shell commands.")
	run("wait", "--fn", `(() => {
		const dialog = document.querySelector('[role=dialog]');
		return dialog.querySelector('textarea')?.value === '!echo not a preset' &&
			dialog.querySelector('input[name=label]')?.value === '你好';
	})()`)
	run("find", "label", "Prompt or slash command", "fill", "Review carefully.\nKeep this line.")
	button("Save")
	run("wait", "--text", "你好")
	run("press", "Escape")
	run("wait", "--fn", `!document.querySelector('[role=dialog]')`)
	if draft := run("get", "value", "#composer-text"); draft != "Review carefully.\nKeep this line." {
		t.Fatalf("saving a preset changed the draft to %q", draft)
	}
	open()
	button("Edit preset 1")
	run("find", "label", "Label", "fill", "你好 renamed")
	button("Save")
	run("wait", "--text", "你好 renamed")
	button("Edit preset 1")
	run("find", "label", "Prompt or slash command", "fill", "Review carefully.")
	button("Save")
	button("Replace preset 1")
	run("wait", "--text", "你好 renamed")
	run("press", "1")
	run("wait", "--fn", `!document.querySelector('[role=dialog]') && document.querySelector('#composer-text').value === 'Review carefully.'`)

	run("fill", "#composer-text", "日本語 $你好 afterwards")
	run("eval", `(() => { const f = document.querySelector('#composer-text'); f.setSelectionRange(7, 7); f.dispatchEvent(new Event('select', { bubbles: true })); })()`)
	run("wait", "--text", "你好 renamed")
	run("press", "Enter")
	run("wait", "--fn", `(() => {
		const f = document.querySelector('#composer-text');
		return f.value === '日本語 Review carefully. afterwards' && f.selectionStart === 21 &&
			!document.querySelector('[aria-label="Attachments"]');
	})()`)
	open()
	button("Delete preset 1")
	button("Delete preset 1")
	run("wait", "--fn", `!!document.querySelector('[role=dialog] button[aria-label="Add preset 1"]:enabled')`)
	run("press", "Escape")
	run("wait", "--fn", `!document.querySelector('[role=dialog]')`)
	run("fill", "#composer-text", "/")
	run("wait", "--fn", `(() => {
		const menu = document.querySelector('[aria-label="Slash commands"]');
		return menu?.textContent.includes('/compact') && !menu.textContent.includes('/presets');
	})()`)
	run("fill", "#composer-text", "")
	run("set", "media", "light")
	run("wait", "--fn", `!matchMedia('(prefers-color-scheme: dark)').matches`)
	setBrowserViewport(t, run, 1280, 900)
}
