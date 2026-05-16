package modes

import (
	"context"
	"fmt"
	"io"
	"os"

	"github.com/noeljackson/pi/internal/agent"
	"github.com/noeljackson/pi/internal/cli"
)

// RunJSON runs a single prompt and writes JSONL agent events to stdout.
func RunJSON(ctx context.Context, opts cli.Options) error {
	if DefaultAgentFactory == nil {
		return ErrNoDefaultAgentFactory
	}
	runner, closer, err := DefaultAgentFactory(opts)
	if err != nil {
		return err
	}
	if closer != nil {
		defer closer.Close()
	}
	return RunJSONWithAgent(ctx, opts, runner, os.Stdout)
}

// RunJSONWithAgent runs JSON mode with an already configured agent.
func RunJSONWithAgent(ctx context.Context, opts cli.Options, runner runner, out io.Writer) error {
	if err := ApplyOptions(runner, opts); err != nil {
		return err
	}
	unsubscribe := runner.Subscribe(func(event agent.Event) {
		data, err := MarshalEvent(event)
		if err != nil {
			return
		}
		_, _ = fmt.Fprintf(out, "%s\n", data)
	})
	defer unsubscribe()
	return runPrompt(ctx, runner, opts)
}
