package main

import (
	"context"
	"fmt"
	"log/slog"
	"os"
	"sort"
	"text/tabwriter"
	"time"

	"github.com/younsl/o/box/kubernetes/external-ebs-autoresizer/internal/awsx"
	"github.com/younsl/o/box/kubernetes/external-ebs-autoresizer/internal/config"
	"github.com/younsl/o/box/kubernetes/external-ebs-autoresizer/internal/policy"
	"github.com/younsl/o/box/kubernetes/external-ebs-autoresizer/internal/pvscan"
)

// configFilePath resolves the default config file: the CONFIG_FILE env, else
// the mounted default path.
func configFilePath() string {
	if p := os.Getenv("CONFIG_FILE"); p != "" {
		return p
	}
	return config.DefaultConfigFile
}

// loadResolver loads the config at path and builds the policy resolver,
// wrapping either failure so the command exits non-zero with a clear message.
func loadResolver(path string) (*config.Config, *policy.Resolver, error) {
	cfg, err := config.Load(path)
	if err != nil {
		return nil, nil, fmt.Errorf("invalid config %s: %w", path, err)
	}
	resolver, err := policy.New(cfg)
	if err != nil {
		return nil, nil, fmt.Errorf("invalid config %s: %w", path, err)
	}
	return cfg, resolver, nil
}

// runValidate loads and validates the config file (including every policy) and
// reports the outcome. It never contacts AWS.
func runValidate(path string) error {
	cfg, _, err := loadResolver(path)
	if err != nil {
		return err
	}
	fmt.Printf("config %s is valid: region=%s, %d named resize %s plus the default\n",
		path, cfg.Region, len(cfg.Policies), pluralize(len(cfg.Policies), "policy", "policies"))
	return nil
}

// runPolicies prints every resize policy and its effective settings, highest
// weight first, then the default policy. When withCount is set it discovers
// target instances via AWS and adds a MATCHED column with the number each
// policy identifies; otherwise it never contacts AWS.
func runPolicies(ctx context.Context, path string, withCount bool) error {
	cfg, resolver, err := loadResolver(path)
	if err != nil {
		return err
	}

	counts := map[string]int{}
	if withCount {
		if counts, err = discoverPolicyCounts(ctx, cfg, resolver); err != nil {
			return err
		}
	}

	names := resolver.Names()
	sort.SliceStable(names, func(i, j int) bool {
		return weightOf(cfg, names[i]) > weightOf(cfg, names[j])
	})

	tw := tabwriter.NewWriter(os.Stdout, 0, 2, 2, ' ', 0)
	header := "POLICY\tWEIGHT\tSELECTOR\tPAUSED\tALERT\tTHRESHOLD%\tGROW\tMAX_GIB"
	if withCount {
		header += "\tMATCHED"
	}
	fmt.Fprintln(tw, header)
	row := func(name, weight, selector string, eff policy.Effective) {
		fmt.Fprintf(tw, "%s\t%s\t%s\t%t\t%t\t%d\t%s\t%d",
			name, weight, selector, eff.Paused, eff.AlertEnabled, eff.UsageThresholdPercent, growSummary(eff), eff.MaxVolumeSizeGiB)
		if withCount {
			fmt.Fprintf(tw, "\t%d", counts[name])
		}
		fmt.Fprintln(tw)
	}
	for _, name := range names {
		eff, _ := resolver.EffectiveOf(name)
		row(name, fmt.Sprintf("%d", weightOf(cfg, name)), selectorOf(cfg, name), eff)
	}
	row(policy.DefaultPolicyName, "-", "(instances matching no policy)", resolver.Default())
	return tw.Flush()
}

// discoverInstances initializes AWS clients and discovers the target instances
// for cfg, bounded by a 60s timeout. It is the shared discovery step behind
// every CLI subcommand that contacts AWS.
func discoverInstances(ctx context.Context, cfg *config.Config) ([]awsx.Instance, error) {
	ctx, cancel := context.WithTimeout(ctx, 60*time.Second)
	defer cancel()
	clients, err := awsx.New(ctx, cfg.Region)
	if err != nil {
		return nil, fmt.Errorf("initialize AWS clients: %w", err)
	}
	filters := make([]awsx.TagFilter, len(cfg.TagFilters))
	for i, f := range cfg.TagFilters {
		filters[i] = awsx.TagFilter{Key: f.Key, Value: f.Value}
	}
	instances, err := clients.DescribeTargetInstances(ctx, filters, cfg.ExcludeEKSNodes)
	if err != nil {
		return nil, fmt.Errorf("discover instances: %w", err)
	}
	return instances, nil
}

// discoverPolicyCounts discovers target instances and tallies how many each
// policy matches, seeding every policy (and default) to 0.
func discoverPolicyCounts(ctx context.Context, cfg *config.Config, resolver *policy.Resolver) (map[string]int, error) {
	instances, err := discoverInstances(ctx, cfg)
	if err != nil {
		return nil, err
	}
	counts := map[string]int{policy.DefaultPolicyName: 0}
	for _, name := range resolver.Names() {
		counts[name] = 0
	}
	for _, inst := range instances {
		counts[resolver.Resolve(inst.Name, inst.Tags).Policy]++
	}
	return counts, nil
}

// runInstances discovers target instances via AWS and lists them grouped by the
// policy each one matches, so operators can confirm a policy's reach before it
// takes effect.
func runInstances(ctx context.Context, path string) error {
	cfg, resolver, err := loadResolver(path)
	if err != nil {
		return err
	}

	instances, err := discoverInstances(ctx, cfg)
	if err != nil {
		return err
	}

	byPolicy := map[string][]awsx.Instance{}
	for _, inst := range instances {
		eff := resolver.Resolve(inst.Name, inst.Tags)
		byPolicy[eff.Policy] = append(byPolicy[eff.Policy], inst)
	}

	tw := tabwriter.NewWriter(os.Stdout, 0, 2, 2, ' ', 0)
	fmt.Fprintln(tw, "POLICY\tINSTANCE_ID\tNAME\tROOT_VOLUME\tSIZE_GIB")
	for _, name := range append(resolver.Names(), policy.DefaultPolicyName) {
		list := byPolicy[name]
		if len(list) == 0 {
			fmt.Fprintf(tw, "%s\t(none)\t\t\t\n", name)
			continue
		}
		for _, inst := range list {
			fmt.Fprintf(tw, "%s\t%s\t%s\t%s\t%d\n", name, inst.ID, inst.Name, inst.RootVolumeID, inst.RootVolumeSizeGiB)
		}
	}
	if err := tw.Flush(); err != nil {
		return err
	}
	fmt.Printf("\n%d %s discovered in %s\n", len(instances), pluralize(len(instances), "instance", "instances"), cfg.Region)
	return nil
}

// pluralize returns singular when n == 1, else plural.
func pluralize(n int, singular, plural string) string {
	if n == 1 {
		return singular
	}
	return plural
}

// growSummary renders the growth setting compactly for the policy table.
func growSummary(eff policy.Effective) string {
	if eff.GrowMode == config.GrowModeAbsolute {
		return fmt.Sprintf("absolute +%dGiB", eff.GrowAmountGiB)
	}
	return fmt.Sprintf("percent +%d%%", eff.GrowPercent)
}

// weightOf returns the configured weight of a named policy (0 if not found).
func weightOf(cfg *config.Config, name string) int {
	for _, p := range cfg.Policies {
		if p.Name == name {
			return p.Weight
		}
	}
	return 0
}

// selectorOf renders a policy's instanceSelector compactly for the table.
func selectorOf(cfg *config.Config, name string) string {
	for _, p := range cfg.Policies {
		if p.Name != name {
			continue
		}
		parts := ""
		if len(p.InstanceSelector.Tags) > 0 {
			keys := make([]string, 0, len(p.InstanceSelector.Tags))
			for k := range p.InstanceSelector.Tags {
				keys = append(keys, k)
			}
			sort.Strings(keys)
			for _, k := range keys {
				if parts != "" {
					parts += ","
				}
				parts += k + "=" + p.InstanceSelector.Tags[k]
			}
		}
		if p.InstanceSelector.NameRegex != "" {
			if parts != "" {
				parts += " & "
			}
			parts += "name~" + p.InstanceSelector.NameRegex
		}
		return parts
	}
	return ""
}

// runUnused lists the PersistentVolumeClaims and PersistentVolumes no workload
// is using, grouped by kind and sorted longest-unused first. It reads the
// Kubernetes API and never writes: unlike the scanner loop it annotates nothing,
// so it is safe to run against a cluster where the loop is still disabled.
//
// The clock behind UNUSED_FOR lives in the annotation the loop writes. Without
// that loop running there is nothing to read, so every object reports as unused
// since now; the classification itself is unaffected.
func runUnused(ctx context.Context, path string, all bool) error {
	cfg, _, err := loadResolver(path)
	if err != nil {
		return err
	}
	kube, err := pvscan.NewClient()
	if err != nil {
		return fmt.Errorf("in-cluster Kubernetes access is required to scan volumes: %w", err)
	}
	scanner := pvscan.New(cfg.DryRun, kube, discardRecorder{}, nil, slog.New(slog.DiscardHandler))

	ctx, cancel := context.WithTimeout(ctx, 60*time.Second)
	defer cancel()
	findings, err := scanner.Findings(ctx)
	if err != nil {
		return err
	}

	unused := make([]pvscan.Finding, 0, len(findings))
	for _, f := range findings {
		if f.Unused && (all || f.Reportable) {
			unused = append(unused, f)
		}
	}
	sort.SliceStable(unused, func(i, j int) bool {
		if unused[i].Kind != unused[j].Kind {
			return unused[i].Kind < unused[j].Kind
		}
		return unused[i].Age > unused[j].Age
	})

	tw := tabwriter.NewWriter(os.Stdout, 0, 2, 2, ' ', 0)
	fmt.Fprintln(tw, "KIND\tNAMESPACE\tNAME\tREASON\tUNUSED_FOR\tCAPACITY\tSTORAGE_CLASS\tEBS_VOLUME\tBOUND_TO")
	var total int64
	for _, f := range unused {
		total += f.CapacityBytes
		fmt.Fprintf(tw, "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n",
			f.Kind, orDash(f.Namespace), f.Name, f.Reason, f.Age.Round(time.Minute),
			humanBytes(f.CapacityBytes), orDash(f.StorageClass), orDash(f.VolumeID), orDash(f.BoundTo()))
	}
	if len(unused) == 0 {
		fmt.Fprintln(tw, "(none)\t\t\t\t\t\t\t\t")
	}
	if err := tw.Flush(); err != nil {
		return err
	}
	fmt.Printf("\n%d unused %s holding %s, out of %d objects scanned (minUnusedAge %s)\n",
		len(unused), pluralize(len(unused), "object", "objects"), humanBytes(total),
		len(findings), pvscan.MinUnusedAge)
	return nil
}

// discardRecorder satisfies pvscan.Recorder for the CLI, which reports to stdout
// rather than to the metrics registry the daemon serves.
type discardRecorder struct{}

func (discardRecorder) ResetUnusedVolumes()                                      {}
func (discardRecorder) ObserveUnusedPVC(_, _, _, _, _, _ string, _, _ float64)   {}
func (discardRecorder) ObserveUnusedPV(_, _, _, _, _, _, _ string, _, _ float64) {}
func (discardRecorder) ObserveUnusedSummary(_, _ string, _ int, _ int64)         {}
func (discardRecorder) ObserveError(string)                                      {}

// orDash renders an empty column as a dash, so a blank cell always means "no
// value" rather than a rendering slip.
func orDash(s string) string {
	if s == "" {
		return "-"
	}
	return s
}

// humanBytes renders a byte count in the binary units Kubernetes capacities are
// written in, so a column lines up with what kubectl shows.
func humanBytes(b int64) string {
	const unit = 1024
	if b < unit {
		return fmt.Sprintf("%dB", b)
	}
	div, exp := int64(unit), 0
	for n := b / unit; n >= unit && exp < 4; n /= unit {
		div *= unit
		exp++
	}
	return fmt.Sprintf("%.1f%ci", float64(b)/float64(div), "KMGTP"[exp])
}
