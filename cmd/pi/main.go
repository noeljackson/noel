package main

import (
	"context"
	"fmt"
	"os"
	"strings"

	"github.com/noeljackson/pi/internal/agent"
	anthropicprovider "github.com/noeljackson/pi/internal/anthropic"
	"github.com/noeljackson/pi/internal/session"
)

const (
	defaultModel     = "claude-sonnet-4-6"
	defaultMaxTokens = 4096
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func run() error {
	apiKey := os.Getenv("ANTHROPIC_API_KEY")
	if apiKey == "" {
		return fmt.Errorf("ANTHROPIC_API_KEY is not set")
	}

	model := os.Getenv("PI_MODEL")
	if model == "" {
		model = defaultModel
	}

	args := os.Args[1:]
	if len(args) == 0 {
		return fmt.Errorf("usage: pi <prompt> or pi --resume <session-id>")
	}

	provider := anthropicprovider.NewClient(apiKey)
	emit := func(event agent.Event) {
		if update, ok := event.(agent.MessageUpdateEvent); ok && update.Delta.TextDelta != "" {
			fmt.Fprint(os.Stdout, update.Delta.TextDelta)
		}
	}

	if args[0] == "--resume" {
		if len(args) != 2 {
			return fmt.Errorf("usage: pi --resume <session-id>")
		}
		sessionID := args[1]
		messages, err := session.LoadMessages(sessionID)
		if err != nil {
			return err
		}
		writer, err := session.OpenWriter(sessionID)
		if err != nil {
			return err
		}
		defer writer.Close()
		_, err = agent.Continue(context.Background(), loopConfig(provider, model, writer), messages, emit)
		if err == nil {
			fmt.Fprintln(os.Stdout)
		}
		return err
	}

	prompt := strings.Join(args, " ")
	writer, err := session.NewWriter()
	if err != nil {
		return err
	}
	defer writer.Close()

	_, err = agent.Run(context.Background(), loopConfig(provider, model, writer), agent.UserMessage{
		Content: []agent.Content{agent.TextContent{Text: prompt}},
	}, emit)
	if err == nil {
		fmt.Fprintln(os.Stdout)
	}
	return err
}

func loopConfig(provider agent.Provider, model string, writer agent.SessionWriter) agent.LoopConfig {
	return agent.LoopConfig{
		Provider:      provider,
		Model:         model,
		MaxTokens:     defaultMaxTokens,
		SessionWriter: writer,
	}
}
