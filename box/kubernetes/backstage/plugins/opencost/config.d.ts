export interface Config {
  opencost?: {
    /**
     * IANA timezone for billing day boundaries (e.g. Asia/Seoul).
     * @visibility frontend
     */
    timezone?: string;
    /**
     * List of OpenCost clusters to display
     * @visibility frontend
     */
    clusters?: Array<{
      /** @visibility frontend */
      name: string;
      /**
       * Operator-facing cluster name shown in the UI and written to the Cluster column of CSV exports
       * @visibility frontend
       */
      alias?: string;
    }>;
  };
}
