package components

import (
	"fmt"
	"strings"

	"github.com/charmbracelet/lipgloss"
	"github.com/noeljackson/pi/internal/agent"
)

var customBoxStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("252")).Background(lipgloss.Color("235")).Padding(0, 1)

func BranchSummaryView(msg agent.BranchSummaryMessage, width int, expanded bool) string {
	return summaryBox("[branch]", "Branch summary", msg.Summary, 0, width, expanded)
}

func CompactionSummaryView(msg agent.CompactionSummaryMessage, width int, expanded bool) string {
	title := fmt.Sprintf("Compacted from %s tokens", formatTokens(msg.TokensBefore))
	return summaryBox("[compaction]", title, msg.Summary, msg.TokensBefore, width, expanded)
}

func SkillInvocationView(name string, content string, width int, expanded bool) string {
	if expanded {
		return customBoxStyle.Width(max(0, width-2)).Render("[skill]\n" + MarkdownView("**"+name+"**\n\n"+content, max(1, width-4)))
	}
	return customBoxStyle.Width(max(0, width-2)).Render("[skill] " + name + " (expand for details)")
}

func ThinkingView(content agent.ThinkingContent, width int, expanded bool) string {
	text := strings.TrimSpace(content.Thinking)
	if text == "" && content.Redacted {
		text = "[redacted thinking]"
	}
	if !expanded {
		return messageThinkingStyle.Render("[thinking...]")
	}
	return messageThinkingStyle.Render(MarkdownView(text, width))
}

func BashExecutionView(msg agent.BashExecutionMessage, width int, expanded bool) string {
	header := toolTitleStyle.Render("$ " + msg.Command)
	output := msg.Output
	if output == "" {
		output = strings.TrimRight(strings.Join(nonEmpty(msg.Stdout, msg.Stderr), "\n"), "\n")
	}
	lines := []string{header}
	if output != "" {
		outLines := strings.Split(strings.TrimRight(output, "\n"), "\n")
		if !expanded && len(outLines) > maxToolBodyLines {
			outLines = outLines[len(outLines)-maxToolBodyLines:]
			lines = append(lines, toolDimStyle.Render("... more output"))
		}
		for _, line := range outLines {
			lines = append(lines, toolDimStyle.Render(truncateWidth(line, max(1, width-4))))
		}
	}
	var status []string
	if msg.Cancelled {
		status = append(status, "cancelled")
	}
	if msg.ExitCode != nil {
		status = append(status, fmt.Sprintf("exit %d", *msg.ExitCode))
	}
	if msg.Truncated && msg.FullOutputPath != "" {
		status = append(status, "full output: "+msg.FullOutputPath)
	}
	if len(status) > 0 {
		lines = append(lines, toolDimStyle.Render(strings.Join(status, " | ")))
	}
	style := toolBorderStyle
	if width > 4 {
		style = style.Width(width - 4)
	}
	return style.Render(strings.Join(lines, "\n"))
}

func summaryBox(label, title, summary string, _ int, width int, expanded bool) string {
	body := label + "\n\n"
	if expanded {
		body += MarkdownView("**"+title+"**\n\n"+summary, max(1, width-4))
	} else {
		body += title + " (expand for details)"
	}
	style := customBoxStyle
	if width > 2 {
		style = style.Width(width - 2)
	}
	return style.Render(body)
}

func nonEmpty(values ...string) []string {
	out := make([]string, 0, len(values))
	for _, value := range values {
		if value != "" {
			out = append(out, value)
		}
	}
	return out
}
