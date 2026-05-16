package modes

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"

	"github.com/noeljackson/pi/internal/agent"
	"github.com/noeljackson/pi/internal/cli"
)

var ErrNoDefaultAgentFactory = errors.New("no default agent factory configured")

// AgentFactory constructs an agent for the package-level RunPrint helper.
type AgentFactory func(cli.Options) (*agent.Agent, io.Closer, error)

// DefaultAgentFactory is used by RunPrint.
var DefaultAgentFactory AgentFactory

// RunPrint runs a single prompt in text print mode.
func RunPrint(ctx context.Context, opts cli.Options) error {
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
	return RunPrintWithAgent(ctx, opts, runner, os.Stdout)
}

// RunPrintWithAgent runs print mode with an already configured agent.
func RunPrintWithAgent(ctx context.Context, opts cli.Options, runner runner, out io.Writer) error {
	if err := ApplyOptions(runner, opts); err != nil {
		return err
	}
	unsubscribe := runner.Subscribe(func(event agent.Event) {
		update, ok := event.(agent.MessageUpdateEvent)
		if !ok || update.Delta.TextDelta == "" {
			return
		}
		_, _ = fmt.Fprint(out, update.Delta.TextDelta)
	})
	defer unsubscribe()
	if err := runPrompt(ctx, runner, opts); err != nil {
		return err
	}
	_, err := fmt.Fprintln(out)
	return err
}
