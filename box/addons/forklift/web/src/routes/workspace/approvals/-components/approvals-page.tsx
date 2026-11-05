import { useState } from "react";
import { useNavigate, useSearch } from "@tanstack/react-router";

import { Button } from "@/components/ui/button";
import { PageDescription, PageHeader } from "@/components/app-ui/page";
import { Select } from "@/components/app-ui/select";
import { useAuth } from "@/authContext";
import { useTranslation } from "@/lib/i18n";
import { AddRuleModal } from "@/routes/workspace/approvals/-components/add-rule-modal";
import { ApprovalList } from "@/routes/workspace/approvals/-components/approval-list";
import { useApprovalRepoOptions } from "@/routes/workspace/approvals/-hooks/use-approval-repo-options";
import { canReviewApprovals } from "@/utils/permissions";

// Approvals is the cross-repository work queue for package approval requests:
// security engineers review demand here, approve or reject with a note, and
// pre-approve packages before anyone asks. Per-repository views reuse
// ApprovalList from the repository detail's Approvals tab.
export function ApprovalsPage() {
  const { t } = useTranslation();
  const { me } = useAuth();
  const repo = String((useSearch({ strict: false }) as { repo?: unknown }).repo ?? "");
  const navigate = useNavigate();
  const setRepo = (value: string) =>
    void navigate({
      to: ".",
      search: ((prev: Record<string, unknown>) => ({ ...prev, repo: value || undefined, page: undefined })) as never,
      replace: true,
    });
  const [isAddingRule, setIsAddingRule] = useState(false);
  const { idsByName, names } = useApprovalRepoOptions();

  return (
    <div data-testid="page-approvals">
      <PageHeader
        title={t("approval.title")}
        actions={
          canReviewApprovals(me)
            ? <Button onClick={() => setIsAddingRule(true)}>{t("approval.add-rule")}</Button>
            : undefined
        }
      />
      <PageDescription>{t("approval.queue-description")}</PageDescription>
      <div>
        <ApprovalList
          repo={repo}
          showRepo
          repoNames={names}
          repoIds={idsByName}
          filters={
            <Select
              className="w-full sm:w-[200px]"
              value={repo}
              onChange={setRepo}
              options={[
                { value: "", label: "all repositories" },
                ...names.map((name) => ({ value: name, label: name })),
              ]}
            />
          }
        />
      </div>
      {isAddingRule && (
        <AddRuleModal
          repoNames={names}
          // The rule mutation invalidates the queue, so closing is all that is
          // left to do here.
          onDone={() => setIsAddingRule(false)}
          onCancel={() => setIsAddingRule(false)}
        />
      )}
    </div>
  );
}
