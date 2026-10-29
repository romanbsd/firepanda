FROM node:26-alpine
WORKDIR /app
COPY package.json pnpm-lock.yaml ./
RUN npm install --global pnpm@12.16.1 && pnpm install --prod --frozen-lockfile
COPY src ./src
ENV HOST=0.0.0.0 PORT=3000 LIGHTPANDA_CDP_URL=http://lightpanda:9222
EXPOSE 3000
CMD ["npm", "start"]
