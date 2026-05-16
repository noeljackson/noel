package benchmarks

import (
	"bytes"
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/noeljackson/pi/internal/agent"
	"github.com/noeljackson/pi/internal/session"
	"github.com/noeljackson/pi/internal/tui"
	"github.com/noeljackson/pi/internal/tui/components"
)

const (
	coldStartLimit        = 50 * time.Millisecond
	buildLimit            = time.Second
	sessionResumeLimit    = 100 * time.Millisecond
	renderTUILimit        = 150 * time.Millisecond
	markdownRenderLimit   = time.Millisecond
	validateToolArgsLimit = time.Millisecond
)

func BenchmarkColdStart(b *testing.B) {
	goTool := goToolPath(b)
	root := repoRoot()
	tempDir := executableTempDir(b, root)
	binary := filepath.Join(tempDir, "pi")
	runCommand(b, root, goTool, "build", "-o", binary, "./cmd/pi")

	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		start := time.Now()
		cmd := exec.Command(binary, "--startup-probe", "--quiet-startup")
		cmd.Dir = root
		output, err := cmd.CombinedOutput()
		if err != nil {
			b.Fatalf("startup probe failed: %v\n%s", err, output)
		}
		elapsed := time.Since(start)
		if !bytes.Contains(output, []byte("prompt-ready")) {
			b.Fatalf("startup probe output = %q", output)
		}
		if elapsed > coldStartLimit {
			b.Fatalf("cold start took %s, limit %s", elapsed, coldStartLimit)
		}
	}
}

func BenchmarkSessionResume(b *testing.B) {
	store := session.NewJSONLStore(b.TempDir())
	sess, err := store.Create("/tmp/project")
	if err != nil {
		b.Fatal(err)
	}
	for i := 0; i < 50; i++ {
		if err := sess.AppendMessage(agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "hello"}}}); err != nil {
			b.Fatal(err)
		}
		if err := sess.AppendMessage(agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "world"}}}); err != nil {
			b.Fatal(err)
		}
	}
	id := sess.ID()
	if err := sess.Close(); err != nil {
		b.Fatal(err)
	}

	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		start := time.Now()
		opened, err := store.Open(id)
		if err != nil {
			b.Fatal(err)
		}
		if _, err := opened.Messages(); err != nil {
			b.Fatal(err)
		}
		if err := opened.Close(); err != nil {
			b.Fatal(err)
		}
		if elapsed := time.Since(start); elapsed > sessionResumeLimit {
			b.Fatalf("session resume took %s, limit %s", elapsed, sessionResumeLimit)
		}
	}
}

func BenchmarkBuild(b *testing.B) {
	goTool := goToolPath(b)
	root := repoRoot()
	out := filepath.Join(executableTempDir(b, root), "pi")

	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		start := time.Now()
		runCommand(b, root, goTool, "build", "-o", out, "./cmd/pi")
		if elapsed := time.Since(start); elapsed > buildLimit {
			b.Fatalf("go build took %s, limit %s", elapsed, buildLimit)
		}
	}
}

func BenchmarkRenderTUI(b *testing.B) {
	messages := make([]agent.Message, 0, 1000)
	for i := 0; i < 500; i++ {
		messages = append(messages,
			agent.UserMessage{Content: []agent.Content{agent.TextContent{Text: "render this message"}}},
			agent.AssistantMessage{Content: []agent.Content{agent.TextContent{Text: "assistant response with enough text to wrap across the viewport"}}},
		)
	}
	model := tui.New(tui.Options{Messages: messages, Model: "benchmark"})
	updated, _ := model.Update(tea.WindowSizeMsg{Width: 100, Height: 40})
	model = updated.(tui.Model)

	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		_ = model.View()
	}
	if avg := b.Elapsed() / time.Duration(b.N); avg > renderTUILimit {
		b.Fatalf("TUI render took %s average, limit %s", avg, renderTUILimit)
	}
}

func BenchmarkMarkdownRender(b *testing.B) {
	content := []agent.Content{agent.TextContent{Text: mixedMarkdown4KB()}}
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		_ = components.AssistantMessageView(content)
	}
	if avg := b.Elapsed() / time.Duration(b.N); avg > markdownRenderLimit {
		b.Fatalf("markdown render took %s average, limit %s", avg, markdownRenderLimit)
	}
}

func BenchmarkValidateToolArgs(b *testing.B) {
	schema := json.RawMessage(`{"type":"object","required":["path","limit"],"properties":{"path":{"type":"string"},"limit":{"type":"integer"},"enabled":{"type":"boolean"}}}`)
	payloads := make([]json.RawMessage, 100)
	for i := range payloads {
		payloads[i] = json.RawMessage(`{"path":"file.txt","limit":"12","enabled":"true"}`)
	}

	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		for _, payload := range payloads {
			if err := agent.ValidateToolArguments(schema, payload); err != nil {
				b.Fatal(err)
			}
		}
	}
	perCall := b.Elapsed() / time.Duration(b.N*len(payloads))
	if perCall > validateToolArgsLimit {
		b.Fatalf("tool arg validation took %s per call, limit %s", perCall, validateToolArgsLimit)
	}
}

func goToolPath(b *testing.B) string {
	b.Helper()
	path, err := exec.LookPath("go")
	if err != nil {
		b.Skip("go toolchain not found")
	}
	return path
}

func repoRoot() string {
	_, file, _, _ := runtime.Caller(0)
	return filepath.Clean(filepath.Join(filepath.Dir(file), "..", ".."))
}

func runCommand(b *testing.B, dir string, name string, args ...string) {
	b.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, name, args...)
	cmd.Dir = dir
	output, err := cmd.CombinedOutput()
	if err != nil {
		b.Fatalf("%s %v failed: %v\n%s", name, args, err, output)
	}
}

func executableTempDir(b *testing.B, root string) string {
	b.Helper()
	dir, err := os.MkdirTemp(root, ".bench-")
	if err != nil {
		b.Fatal(err)
	}
	b.Cleanup(func() { _ = os.RemoveAll(dir) })
	return dir
}

func mixedMarkdown4KB() string {
	chunk := "## Heading\n\nText with **bold**, `code`, a [link](https://example.com), and a list:\n\n- one\n- two\n\n```go\nfmt.Println(\"hello\")\n```\n\n"
	var buf bytes.Buffer
	for buf.Len() < 4096 {
		buf.WriteString(chunk)
	}
	return buf.String()[:4096]
}
