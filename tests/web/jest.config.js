// Browser E2E tests boot a real `mj server` and drive real Chrome, so they
// run serially and get a generous timeout. Not part of `cargo test` / CI —
// run manually with `npm test` from this directory.
module.exports = {
  testEnvironment: "node",
  testMatch: ["**/*.test.js"],
  testTimeout: 60000,
  maxWorkers: 1,
};
