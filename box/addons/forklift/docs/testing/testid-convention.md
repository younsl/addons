# `data-testid` convention

The handles the browser tests hold on to. They exist so a test survives a
change to the wording on screen (i18n) or to the DOM around it: **what a
selector depends on is stated deliberately rather than inferred.**

Finding an element by its text breaks in two ways: when a translation changes,
and when the same word appears twice (the panel titles on the repository
statistics tab, for instance, are plain `<div>`s and do not even carry a
heading role).

## Names

| Shape | Where it goes | Example |
|---|---|---|
| `page-<slug>` | One per screen. The top-level element that proves the screen rendered | `page-repositories`, `page-role-new` |
| `tab-<name>` | A tab link | `tab-artifacts`, `tab-security` |
| `panel-<slug>` | A card or section within a screen | `panel-danger-zone`, `panel-assigned-users` |
| `row-<entity>` | A table row, named after the entity itself | `row-maven-hosted` |
| `field-<name>` | A form input | `field-username` |
| `action-<verb>` | A button | `action-create`, `action-delete`, `action-save` |
| `value-<name>` | A reading a test needs to take | `value-pending-count` |

`<slug>` is kebab-case. It is not prefixed with the domain name — the screen it
sits in already says that.

## When to add one

**Add one for**

- the root of a screen (`page-*`), which the smoke tests use
- a number or badge a test has to **read** (`value-*`), which the cross-screen
  freshness checks use
- a row that has to be picked out of many (`row-*`)
- a button whose wording is ambiguous or repeated

**Do not add one for**

- anything an accessibility role and name already identify uniquely. If
  `getByRole("button", { name: "Save changes" })` matches exactly one element,
  a testid is noise
- purely decorative elements

The principle: **a testid goes where role and name cannot reach.** Putting one
on everything leaves the accessibility tree to rot, because nothing depends on
it any more.

## Examples

```tsx
// The root of a screen.
<div data-testid="page-repositories">

// A value the freshness checks have to read.
<Badge data-testid="value-pending-approvals" variant="warning">{pending}</Badge>

// A row, identified by name. Absolute counts are unusable under parallel
// execution, so this is the only way a test can look at its own row.
<TableRow data-testid={`row-${repo.name}`}>
```
