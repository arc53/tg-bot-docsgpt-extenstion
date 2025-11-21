# Telegram DocsGPT extension

This repository contains the source code for a Telegram bot that leverages DocsGPT to provide intelligent responses to user queries. This bot is an extension for [DocsGPT](https://www.docsgpt.cloud/).

## Features
- Responds to user queries with intelligent answers using DocsGPT.
- Maintains conversation history for context-aware responses.
- Supports multiple storage backends for conversation history (in-memory or MongoDB).
- Easily deployable using Docker.

## Prerequisites
Before you begin, ensure you have met the following requirements:
- You have registered a bot with [BotFather](https://core.telegram.org/bots#botfather) on Telegram and obtained a `TOKEN`.
- You have created an API key on [DocsGPT](https://www.docsgpt.cloud/) to access your AI's data and prompt.

## Installation

### Using Python

1. Clone the repository:
    ```bash
    git clone https://github.com/arc53/tg-bot-docsgpt-extenstion.git
    cd tg-bot-docsgpt-extenstion
    ```

2. Set up a virtual environment:
    ```bash
    python3 -m venv venv
    ```
    - On macOS and Linux:
      ```bash
      source venv/bin/activate
      ```
    - On Windows:
      ```bash
      .\venv\Scripts\activate
      ```

3. Install dependencies:
    ```bash
    pip install -r requirements.txt
    ```

4. Create a `.env` file in the project directory and add your environment variables:
    ```plaintext
    TELEGRAM_BOT_TOKEN=<your-telegram-bot-token>
    API_KEY=<your-api-key>
    # Optional: Multiple Agents
    # API_KEY_SUPPORT=<support-agent-api-key>
    # API_KEY_SALES=<sales-agent-api-key>
    # Optional: Storage Configuration (Defaults to in-memory)
    # STORAGE_TYPE=mongodb
    # MONGODB_URI=<your-mongodb-connection-string>
    # MONGODB_DB_NAME=telegram_bot_memory
    # MONGODB_COLLECTION_NAME=chat_histories
    ```
    - `TELEGRAM_BOT_TOKEN`: Your Telegram bot token from BotFather.
    - `API_KEY`: Your default DocsGPT Agent API key. If not set, the bot will fallback to the first available agent key.
    - `API_KEY_<AGENT_NAME>`: (Optional) API keys for specific agents. Replace `<AGENT_NAME>` with the agent's name (e.g., `SUPPORT`, `SALES`).
    - `STORAGE_TYPE`: (Optional) Specifies where to store conversation history. Defaults to `memory`. Set to `mongodb` to use MongoDB.
    - `MONGODB_URI`: (Required if `STORAGE_TYPE=mongodb`) Your MongoDB connection string.
    - `MONGODB_DB_NAME`: (Optional, defaults to `telegram_bot_memory`) The name of the MongoDB database.
    - `MONGODB_COLLECTION_NAME`: (Optional, defaults to `chat_histories`) The name of the MongoDB collection.

5. Run the bot:
    ```bash
    python bot.py
    ```

### Using Docker

1. Clone the repository:
    ```bash
    git clone https://github.com/arc53/tg-bot-docsgpt-extenstion.git
    cd tg-bot-docsgpt-extenstion
    ```

2. Build the Docker image:
    ```bash
    docker build -t telegram-gpt-bot .
    ```

3. Create a `.env` file in the project directory and add your environment variables:
    ```plaintext
    TELEGRAM_BOT_TOKEN=<your-telegram-bot-token>
    API_KEY=<your-api-key>
    # Optional: Multiple Agents
    # API_KEY_SUPPORT=<support-agent-api-key>
    # API_KEY_SALES=<sales-agent-api-key>
    # Optional: Storage Configuration (Defaults to in-memory)
    # STORAGE_TYPE=mongodb
    # MONGODB_URI=<your-mongodb-connection-string>
    # MONGODB_DB_NAME=telegram_bot_memory
    # MONGODB_COLLECTION_NAME=chat_histories
    ```
    See the Python installation section above for details on environment variables.

4. Run the Docker container:
    ```bash
    docker run --env-file .env telegram-gpt-bot
    ```

## Usage

### Telegram Commands
- `/start` - Initiates the conversation with the bot.
- `/help` - Provides help information.

### General Conversation
Simply type any message, and the bot will respond with an intelligent answer based on the context of the conversation maintained in `context.chat_data`.

### Multiple Agents
You can configure multiple agents by setting environment variables like `API_KEY_SUPPORT`, `API_KEY_SALES`, etc.

To query a specific agent, start your message with `#` followed by the agent name:
- `#support I have a bug` -> Routes to the agent configured with `API_KEY_SUPPORT`.
- `#sales What is the pricing?` -> Routes to the agent configured with `API_KEY_SALES`.

If no agent tag is provided the bot uses the default `API_KEY`. If `API_KEY` is not set, it falls back to the first available agent.

## File Description
- `bot.py`: The main script for running the bot.
- `requirements.txt`: Python dependencies required by the bot.
- `Dockerfile`: Instructions to build the Docker image.
- `docker-compose.yml`: Docker Compose configuration for easier deployment.
- `.env`: File containing environment variables (not included, must be created).

## License
This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Acknowledgments
- [DocsGPT](https://www.docsgpt.cloud/)
- [DocsGPT Github](https://github.com/arc53/docsgpt)
- [Telegram Bot API](https://core.telegram.org/bots/api)
- [Python Telegram Bot](https://python-telegram-bot.readthedocs.io/)