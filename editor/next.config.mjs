/** @type {import('next').NextConfig} */
const nextConfig = {
  // Route handlers shell out (child_process) and read the local configs/ tree — Node.js runtime
  // (the default) is required; nothing here needs to run on the Edge runtime.
  eslint: { ignoreDuringBuilds: true },
};

export default nextConfig;
