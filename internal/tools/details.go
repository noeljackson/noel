package tools

import "encoding/json"

type BashDetails struct {
	ExitCode    int    `json:"exitCode"`
	Stdout      string `json:"stdout,omitempty"`
	Stderr      string `json:"stderr,omitempty"`
	StdoutBytes int    `json:"stdoutBytes"`
	StderrBytes int    `json:"stderrBytes"`
	Command     string `json:"command"`
	DurationMS  int    `json:"durationMS"`
	OutputFile  string `json:"outputFile,omitempty"`
}

type ReadDetails struct {
	Path      string `json:"path"`
	Lines     int    `json:"lines"`
	Bytes     int    `json:"bytes"`
	Truncated bool   `json:"truncated"`
	StartLine int    `json:"startLine,omitempty"`
}

type WriteDetails struct {
	Path  string `json:"path"`
	Bytes int    `json:"bytes"`
	Lines int    `json:"lines"`
}

type EditDetails struct {
	Path         string     `json:"path"`
	EditsApplied int        `json:"editsApplied"`
	BeforeLines  int        `json:"beforeLines"`
	AfterLines   int        `json:"afterLines"`
	Hunks        []EditHunk `json:"hunks,omitempty"`
}

type EditHunk struct {
	OldStart int    `json:"oldStart,omitempty"`
	OldLines int    `json:"oldLines,omitempty"`
	NewStart int    `json:"newStart,omitempty"`
	NewLines int    `json:"newLines,omitempty"`
	Method   string `json:"method,omitempty"`
	Diff     string `json:"diff,omitempty"`
	OldText  string `json:"oldText,omitempty"`
	NewText  string `json:"newText,omitempty"`
}

type GrepDetails struct {
	Pattern    string   `json:"pattern,omitempty"`
	Files      []string `json:"files,omitempty"`
	Matches    int      `json:"matches"`
	Truncated  bool     `json:"truncated"`
	OutputMode string   `json:"outputMode"`
}

type FindDetails struct {
	Pattern   string `json:"pattern"`
	Hits      int    `json:"hits"`
	Limit     int    `json:"limit"`
	Truncated bool   `json:"truncated"`
}

type LsDetails struct {
	Path      string `json:"path"`
	Entries   int    `json:"entries"`
	Limit     int    `json:"limit"`
	Total     int    `json:"total,omitempty"`
	Truncated bool   `json:"truncated"`
}

func MarshalDetails(details interface{}) (json.RawMessage, error) {
	if details == nil {
		return nil, nil
	}
	encoded, err := json.Marshal(details)
	if err != nil {
		return nil, err
	}
	return encoded, nil
}
