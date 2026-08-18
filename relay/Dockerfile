FROM node:20-alpine
WORKDIR /app
COPY package.json server.js fader.html ./
RUN npm install --production
EXPOSE 8080
CMD ["node", "server.js"]