export interface Config {
  app: {
    /**
     * Feature flags for custom plugins
     * @visibility frontend
     */
    plugins?: {
      /**
       * Enable or disable OpenCost plugin
       * @visibility frontend
       */
      opencost?: boolean;
    };
    /**
     * Internal platform services for developers
     * @visibility frontend
     */
    platforms?: Array<{
      /**
       * Platform name
       * @visibility frontend
       */
      name: string;
      /**
       * Category for grouping
       * @visibility frontend
       */
      category: string;
      /**
       * Platform description
       * @visibility frontend
       */
      description: string;
      /**
       * Platform URL
       * @visibility frontend
       */
      url?: string;
      /**
       * Logo URL
       * @visibility frontend
       */
      logo: string;
      /**
       * Tags (comma-separated)
       * @visibility frontend
       */
      tags?: string;
      /**
       * Marks the platform as heading for retirement. Defaults to false.
       * @visibility frontend
       */
      deprecated?: boolean;
    }>;
  };
}
