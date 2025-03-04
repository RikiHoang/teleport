/*
Copyright 2022 Gravitational, Inc.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

package common

import (
	"context"
	"fmt"
	"strings"
	"time"

	"github.com/gravitational/teleport/api/types"
	"github.com/gravitational/teleport/lib/asciitable"
	"github.com/gravitational/teleport/lib/auth"
	"github.com/gravitational/teleport/lib/service"

	"github.com/gravitational/kingpin"
	"github.com/gravitational/trace"

	"github.com/google/uuid"
)

// AlertCommand implements the `tctl alerts` family of commands.
type AlertCommand struct {
	config *service.Config

	message string

	verbose bool

	alertList   *kingpin.CmdClause
	alertCreate *kingpin.CmdClause
}

// Initialize allows AlertCommand to plug itself into the CLI parser
func (c *AlertCommand) Initialize(app *kingpin.Application, config *service.Config) {
	c.config = config
	alert := app.Command("alerts", "Manage cluster alerts").Alias("alert")

	c.alertList = alert.Command("list", "List cluster alerts").Alias("ls")
	c.alertList.Flag("verbose", "Show detailed alert info").Short('v').BoolVar(&c.verbose)

	c.alertCreate = alert.Command("create", "Create cluster alerts").Hidden()
	c.alertCreate.Arg("message", "Alert body message").Required().StringVar(&c.message)
}

// TryRun takes the CLI command as an argument (like "alerts ls") and executes it.
func (c *AlertCommand) TryRun(ctx context.Context, cmd string, client auth.ClientI) (match bool, err error) {
	switch cmd {
	case c.alertList.FullCommand():
		err = c.List(ctx, client)
	case c.alertCreate.FullCommand():
		err = c.Create(ctx, client)
	default:
		return false, nil
	}
	return true, trace.Wrap(err)
}

func (c *AlertCommand) List(ctx context.Context, client auth.ClientI) error {
	alerts, err := client.GetClusterAlerts(ctx, types.GetClusterAlertsRequest{
		// TODO(fspmarshall): support query parameters
	})
	if err != nil {
		return trace.Wrap(err)
	}

	if len(alerts) == 0 {
		fmt.Println("no alerts")
		return nil
	}

	// sort so that newer/high-severity alerts show up higher.
	types.SortClusterAlerts(alerts)

	if c.verbose {
		table := asciitable.MakeTable([]string{"Severity", "Message", "Created", "Labels"})
		for _, alert := range alerts {
			var labelPairs []string
			for key, val := range alert.Metadata.Labels {
				// alert labels can be displayed unquoted because we enforce a
				// very limited charset.
				labelPairs = append(labelPairs, fmt.Sprintf("%s=%s", key, val))
			}
			table.AddRow([]string{
				alert.Spec.Severity.String(),
				fmt.Sprintf("%q", alert.Spec.Message),
				alert.Spec.Created.Format(time.RFC822),
				strings.Join(labelPairs, ", "),
			})
		}
		fmt.Println(table.AsBuffer().String())
	} else {
		table := asciitable.MakeTable([]string{"Severity", "Message"})
		for _, alert := range alerts {
			table.AddRow([]string{alert.Spec.Severity.String(), fmt.Sprintf("%q", alert.Spec.Message)})
		}
		fmt.Println(table.AsBuffer().String())
	}

	return nil
}

func (c *AlertCommand) Create(ctx context.Context, client auth.ClientI) error {
	alert, err := types.NewClusterAlert(uuid.New().String(), c.message)
	if err != nil {
		return trace.Wrap(err)
	}
	return trace.Wrap(client.UpsertClusterAlert(ctx, alert))
}
