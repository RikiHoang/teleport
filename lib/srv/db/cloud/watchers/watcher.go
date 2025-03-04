/*
Copyright 2021 Gravitational, Inc.

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

package watchers

import (
	"context"
	"time"

	"github.com/gravitational/teleport/api/types"
	"github.com/gravitational/teleport/api/utils"
	"github.com/gravitational/teleport/lib/cloud"
	"github.com/gravitational/teleport/lib/services"

	"github.com/gravitational/trace"
	"github.com/sirupsen/logrus"
)

// WatcherConfig is the cloud watcher configuration.
type WatcherConfig struct {
	// AWSMatchers is a list of matchers for AWS databases.
	AWSMatchers []services.AWSMatcher
	// Clients provides cloud API clients.
	Clients cloud.Clients
	// Interval is the interval between fetches.
	Interval time.Duration
}

// CheckAndSetDefaults validates the config.
func (c *WatcherConfig) CheckAndSetDefaults() error {
	if c.Clients == nil {
		c.Clients = cloud.NewClients()
	}
	if c.Interval == 0 {
		c.Interval = 5 * time.Minute
	}
	return nil
}

// Watcher monitors cloud databases according to the provided selectors.
type Watcher struct {
	// cfg is the watcher config.
	cfg WatcherConfig
	// log is the watcher logger.
	log logrus.FieldLogger
	// ctx is the watcher close context.
	ctx context.Context
	// fetchers fetch databases according to their selectors.
	fetchers []Fetcher
	// databasesC is a channel where fetched databases are sent.
	databasesC chan (types.Databases)
}

// Fetcher fetches cloud databases.
type Fetcher interface {
	// Get returns cloud databases matching the fetcher's selector.
	Get(context.Context) (types.Databases, error)
}

// NewWatcher returns a new instance of a cloud databases watcher.
func NewWatcher(ctx context.Context, config WatcherConfig) (*Watcher, error) {
	if err := config.CheckAndSetDefaults(); err != nil {
		return nil, trace.Wrap(err)
	}
	fetchers, err := makeFetchers(config.Clients, config.AWSMatchers)
	if err != nil {
		return nil, trace.Wrap(err)
	}
	if len(fetchers) == 0 {
		return nil, trace.NotFound("no cloud selectors")
	}
	return &Watcher{
		cfg:        config,
		log:        logrus.WithField(trace.Component, "watcher:cloud"),
		ctx:        ctx,
		fetchers:   fetchers,
		databasesC: make(chan types.Databases),
	}, nil
}

// Start starts fetching cloud databases and sending them to the channel.
//
// TODO(r0mant): In future, instead of (or in addition to) polling, we can
// use a combination of EventBridge (former CloudWatch Events) and SQS/SNS to
// subscribe to events such as created/removed instances and tag changes, but
// this will require Teleport to have more AWS permissions.
func (w *Watcher) Start() {
	ticker := time.NewTicker(w.cfg.Interval)
	defer ticker.Stop()
	w.log.Debugf("Starting cloud databases watcher.")
	w.fetchAndSend()
	for {
		select {
		case <-ticker.C:
			w.fetchAndSend()
		case <-w.ctx.Done():
			w.log.Debugf("Cloud databases watcher done.")
			return
		}
	}
}

// fetchAndSend fetches databases from all fetchers and sends them to the channel.
func (w *Watcher) fetchAndSend() {
	var result types.Databases
	for _, fetcher := range w.fetchers {
		databases, err := fetcher.Get(w.ctx)
		if err != nil {
			// DB agent may have permissions to fetch some databases but not
			// others. This is acceptable, thus continue to other fetchers.
			if trace.IsAccessDenied(err) {
				w.log.WithError(err).Debugf("Skipping fetcher %v.", fetcher)
				continue
			}

			w.log.WithError(err).Errorf("%s failed.", fetcher)
			return
		}
		result = append(result, databases...)
	}
	select {
	case w.databasesC <- result:
	case <-w.ctx.Done():
	}
}

// DatabasesC returns a channel that receives fetched cloud databases.
func (w *Watcher) DatabasesC() <-chan types.Databases {
	return w.databasesC
}

// makeFetchers returns cloud fetchers for the provided matchers.
func makeFetchers(clients cloud.Clients, matchers []services.AWSMatcher) (result []Fetcher, err error) {
	type makeFetcherFunc func(cloud.Clients, string, types.Labels) (Fetcher, error)
	makeFetcherFuncs := map[string][]makeFetcherFunc{
		services.AWSMatcherRDS:         {makeRDSInstanceFetcher, makeRDSAuroraFetcher},
		services.AWSMatcherRedshift:    {makeRedshiftFetcher},
		services.AWSMatcherElastiCache: {makeElastiCacheFetcher},
		services.AWSMatcherMemoryDB:    {makeMemoryDBFetcher},
	}

	for _, matcher := range matchers {
		for _, region := range matcher.Regions {
			for matcherType, makeFetchers := range makeFetcherFuncs {
				if !utils.SliceContainsStr(matcher.Types, matcherType) {
					continue
				}

				for _, makeFetcher := range makeFetchers {
					fetcher, err := makeFetcher(clients, region, matcher.Tags)
					if err != nil {
						return nil, trace.Wrap(err)
					}
					result = append(result, fetcher)
				}
			}
		}
	}
	return result, nil
}

// makeRDSInstanceFetcher returns RDS instance fetcher for the provided region and tags.
func makeRDSInstanceFetcher(clients cloud.Clients, region string, tags types.Labels) (Fetcher, error) {
	rds, err := clients.GetAWSRDSClient(region)
	if err != nil {
		return nil, trace.Wrap(err)
	}

	fetcher, err := newRDSDBInstancesFetcher(rdsFetcherConfig{
		Region: region,
		Labels: tags,
		RDS:    rds,
	})
	if err != nil {
		return nil, trace.Wrap(err)
	}
	return fetcher, nil
}

// makeRDSAuroraFetcher returns RDS Aurora fetcher for the provided region and tags.
func makeRDSAuroraFetcher(clients cloud.Clients, region string, tags types.Labels) (Fetcher, error) {
	rds, err := clients.GetAWSRDSClient(region)
	if err != nil {
		return nil, trace.Wrap(err)
	}

	fetcher, err := newRDSAuroraClustersFetcher(rdsFetcherConfig{
		Region: region,
		Labels: tags,
		RDS:    rds,
	})
	if err != nil {
		return nil, trace.Wrap(err)
	}
	return fetcher, nil
}

// makeRedshiftFetcher returns Redshift fetcher for the provided region and tags.
func makeRedshiftFetcher(clients cloud.Clients, region string, tags types.Labels) (Fetcher, error) {
	redshift, err := clients.GetAWSRedshiftClient(region)
	if err != nil {
		return nil, trace.Wrap(err)
	}
	return newRedshiftFetcher(redshiftFetcherConfig{
		Region:   region,
		Labels:   tags,
		Redshift: redshift,
	})
}

// makeElastiCacheFetcher returns ElastiCache fetcher for the provided region and tags.
func makeElastiCacheFetcher(clients cloud.Clients, region string, tags types.Labels) (Fetcher, error) {
	elastiCache, err := clients.GetAWSElastiCacheClient(region)
	if err != nil {
		return nil, trace.Wrap(err)
	}
	return newElastiCacheFetcher(elastiCacheFetcherConfig{
		Region:      region,
		Labels:      tags,
		ElastiCache: elastiCache,
	})
}

// makeMemoryDBFetcher returns MemoryDB fetcher for the provided region and tags.
func makeMemoryDBFetcher(clients cloud.Clients, region string, tags types.Labels) (Fetcher, error) {
	memorydb, err := clients.GetAWSMemoryDBClient(region)
	if err != nil {
		return nil, trace.Wrap(err)
	}
	return newMemoryDBFetcher(memoryDBFetcherConfig{
		Region:   region,
		Labels:   tags,
		MemoryDB: memorydb,
	})
}
