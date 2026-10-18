// COMMON_TIMEZONES is a shortlist, not the IANA database. It covers the zones a
// team is most likely to schedule against; anything else is typed straight into
// the same field, and the server validates it either way.
//
// The list is filtered against what has been typed rather than shown whole: a
// name like Asia/Seoul is reached by typing "seo", and an unfiltered list would
// make the field behave like a dropdown that ignores the keyboard.
export const COMMON_TIMEZONES = [
  "UTC",
  "Asia/Seoul",
  "Asia/Tokyo",
  "Asia/Shanghai",
  "Asia/Singapore",
  "Asia/Kolkata",
  "Europe/London",
  "Europe/Berlin",
  "Europe/Paris",
  "America/New_York",
  "America/Chicago",
  "America/Los_Angeles",
  "Australia/Sydney",
];
