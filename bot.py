import logging
import httpx
import re
import os
import json
import datetime
from dotenv import load_dotenv
from telegram import ForceReply, Update, User # Import User
from telegram.ext import Application, CommandHandler, ContextTypes, MessageHandler, filters
from telegram.constants import ParseMode
from pymongo import MongoClient
from pymongo.errors import ConnectionFailure, ConfigurationError

logging.basicConfig(
    format="%(asctime)s - %(name)s - %(levelname)s - %(message)s", level=logging.INFO)
logging.getLogger("httpx").setLevel(logging.WARNING)
logger = logging.getLogger(__name__)

load_dotenv()

# --- Configuration ---
API_BASE = os.getenv("API_BASE", "https://gptcloud.arc53.com")
API_URL = API_BASE + "/api/answer"
API_KEY = os.getenv("API_KEY")
TOKEN = os.getenv("TELEGRAM_BOT_TOKEN")

# --- Storage Configuration ---
STORAGE_TYPE = os.getenv("STORAGE_TYPE", "memory") # Default to in-memory
MONGODB_URI = os.getenv("MONGODB_URI") # Required if STORAGE_TYPE is 'mongodb'
MONGODB_DB_NAME = os.getenv("MONGODB_DB_NAME", "telegram_bot_memory")
MONGODB_COLLECTION_NAME = os.getenv("MONGODB_COLLECTION_NAME", "chat_histories")

# --- Global Storage Variables ---
mongo_client = None
mongo_collection = None
in_memory_storage = {} # Used if STORAGE_TYPE is 'memory'

# --- Initialize Storage ---
if STORAGE_TYPE.lower() == "mongodb":
    if not MONGODB_URI:
        logger.error("STORAGE_TYPE is 'mongodb' but MONGODB_URI is not set. Exiting.")
        exit(1) # Or fallback to memory: STORAGE_TYPE = "memory"; logger.warning(...)
    try:
        logger.info(f"Attempting to connect to MongoDB: {MONGODB_URI[:15]}... DB: {MONGODB_DB_NAME}")
        mongo_client = MongoClient(MONGODB_URI, serverSelectionTimeoutMS=5000) # 5 second timeout
        mongo_client.admin.command('ismaster') # Check connection
        db = mongo_client[MONGODB_DB_NAME]
        mongo_collection = db[MONGODB_COLLECTION_NAME]
        logger.info(f"Successfully connected to MongoDB and selected collection '{MONGODB_COLLECTION_NAME}'.")
    except (ConnectionFailure, ConfigurationError) as e:
        logger.error(f"Failed to connect to MongoDB: {e}", exc_info=True)
        logger.warning("Falling back to in-memory storage due to MongoDB connection error.")
        STORAGE_TYPE = "memory"
        mongo_client = None
        mongo_collection = None
    except Exception as e:
        logger.error(f"An unexpected error occurred during MongoDB initialization: {e}", exc_info=True)
        logger.warning("Falling back to in-memory storage.")
        STORAGE_TYPE = "memory"
        mongo_client = None
        mongo_collection = None
elif STORAGE_TYPE.lower() == "memory":
    logger.info("Using in-memory storage for chat history (will be lost on restart).")
else:
    logger.warning(f"Unknown STORAGE_TYPE '{STORAGE_TYPE}'. Defaulting to in-memory storage.")
    STORAGE_TYPE = "memory"

# --- Storage Access Functions ---

async def get_chat_data(chat_id: int) -> dict:
    """
    Fetches chat history, conversation ID, and user info from the configured storage.
    The user info stored pertains to the *last known user* who interacted in this chat.
    """
    chat_id_str = str(chat_id)
    # Default structure now includes user_info
    default_data = {"history": [], "conversation_id": None, "user_info": None}

    if STORAGE_TYPE == "mongodb" and mongo_collection is not None:
        try:
            doc = mongo_collection.find_one({"_id": chat_id_str})

            if doc:
                history = doc.get("conversation_history", [])
                conv_id = doc.get("conversation_id", None)
                user_info = doc.get("user_info", None) # Get user info
                return {"history": history, "conversation_id": conv_id, "user_info": user_info}
            else:
                return default_data
        except Exception as e:
            logger.error(f"MongoDB Error fetching data for chat_id {chat_id_str}: {e}", exc_info=True)
            return default_data # Return default on error
    else:
        # Use .get with default for user_info for backward compatibility if key missing
        data = in_memory_storage.get(chat_id_str, default_data)
        return {
            "history": data.get("history", []),
            "conversation_id": data.get("conversation_id", None),
            "user_info": data.get("user_info", None) # Get user info
        }


async def save_chat_data(chat_id: int, history: list, conversation_id: str | None, user_info: dict | None):
    """
    Saves chat history, conversation ID, and user info to the configured storage.
    """
    chat_id_str = str(chat_id)
    max_history_len_pairs = 10 # Store last 10 pairs (20 messages)
    limited_history = history[-(max_history_len_pairs * 2):]

    if STORAGE_TYPE == "mongodb" and mongo_collection is not None:
        try:
            update_data = {
                "conversation_history": limited_history,
                "conversation_id": conversation_id,
            }
            if user_info:
                update_data["user_info"] = user_info

            update_doc = {
                "$set": update_data,
                "$currentDate": {"last_updated": True}
            }
            mongo_collection.update_one(
                {"_id": chat_id_str},
                update_doc,
                upsert=True # Create document if it doesn't exist
            )
            logger.debug(f"Saved data for chat {chat_id_str} with user_info: {bool(user_info)}")
        except Exception as e:
            logger.error(f"MongoDB Error saving data for chat_id {chat_id_str}: {e}", exc_info=True)
    else:
        if chat_id_str not in in_memory_storage:
             in_memory_storage[chat_id_str] = {} # Initialize if new

        in_memory_storage[chat_id_str].update({ # Use update to merge data
            "history": limited_history,
            "conversation_id": conversation_id,
            "last_updated": datetime.datetime.now(datetime.timezone.utc) # Timestamp for memory store
        })
        if user_info:
            in_memory_storage[chat_id_str]["user_info"] = user_info
        logger.debug(f"Saved in-memory data for chat {chat_id_str} with user_info: {bool(user_info)}")



async def start(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Send a message when the command /start is issued."""
    user = update.effective_user
    await update.message.reply_html(
        rf"Hi {user.mention_html()}! Ask me anything.",
    )

async def help_command(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Send a message when the command /help is issued."""
    help_text = (
        "Here are the available commands:\n"
        "/start - Begin a new conversation with the bot\n"
        "/help - Display this help message\n\n"
        "Just send any text to ask a question based on the configured documents!"
    )
    await update.message.reply_text(help_text)

async def generate_answer(question: str, messages: list, conversation_id: str | None) -> dict:
    """Generates an answer using the external DocsGPT API."""
    if not API_KEY:
        logger.warning("API_KEY is not set. Cannot call DocsGPT API.")
        return {"answer": "Error: Backend API key is not configured.", "conversation_id": conversation_id}

    try:
        formatted_history = format_history_for_api(messages)
        history_json = json.dumps(formatted_history)
    except TypeError as e:
        logger.error(f"Failed to serialize history to JSON: {e}. History: {messages}", exc_info=True)
        history_json = json.dumps([])

    payload = {
        "question": question,
        "api_key": API_KEY,
        "history": history_json,
        "conversation_id": conversation_id
    }
    headers = {
        "Content-Type": "application/json; charset=utf-8"
    }
    timeout = 120.0
    default_error_msg = "Sorry, I couldn't get an answer from the backend service."

    try:
        async with httpx.AsyncClient() as client:
            response = await client.post(API_URL, json=payload, headers=headers, timeout=timeout)
            response.raise_for_status() # Raise HTTPStatusError for 4xx/5xx

            data = response.json()
            answer = data.get("answer", default_error_msg)
            returned_conv_id = data.get("conversation_id", conversation_id)
            return {"answer": answer, "conversation_id": returned_conv_id}

    except httpx.HTTPStatusError as exc:
        error_details = f"Status {exc.response.status_code}"
        try:
           error_body = exc.response.json()
           error_details += f" - {error_body.get('detail', exc.response.text)}"
        except json.JSONDecodeError:
            error_details += f" - {exc.response.text}"
        logger.error(f"HTTP error calling DocsGPT API: {error_details}")
        return {"answer": f"{default_error_msg} (Error: {exc.response.status_code})", "conversation_id": conversation_id}
    except httpx.RequestError as exc:
        logger.error(f"Network error calling DocsGPT API: {exc}")
        return {"answer": f"{default_error_msg} (Network Error)", "conversation_id": conversation_id}
    except json.JSONDecodeError as exc:
        logger.error(f"Failed to decode JSON response from DocsGPT API: {exc}. Response text: {response.text}", exc_info=True) # Log response text
        return {"answer": f"{default_error_msg} (Invalid Response Format)", "conversation_id": conversation_id}
    except Exception as e:
        logger.error(f"Unexpected error in generate_answer: {e}", exc_info=True)
        return {"answer": f"{default_error_msg} (Unexpected Error)", "conversation_id": conversation_id}


async def echo(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Handles non-command messages: get history, query API, save history & user info, reply."""
    if not update.message or not update.message.text or not update.effective_chat or not update.effective_user:
        logger.warning("Echo handler received an update without message, text, chat, or user.")
        return # Ignore updates without essential info

    chat_id = update.effective_chat.id
    user = update.effective_user
    question = update.message.text
    logger.info(f"Received message from user {user.id} ({user.username or user.first_name}) in chat_id {chat_id}")


    user_info_dict = {
        "id": user.id,
        "first_name": user.first_name,
        "last_name": user.last_name,
        "username": user.username,
        "is_bot": user.is_bot,
        "language_code": user.language_code
    }
    user_info_dict = {k: v for k, v in user_info_dict.items() if v is not None}

    chat_data = await get_chat_data(chat_id)
    current_history = chat_data["history"]
    current_conversation_id = chat_data["conversation_id"]

    current_history.append({"role": "user", "content": question})

    response_doc = await generate_answer(question, current_history, current_conversation_id)
    answer = response_doc["answer"]
    new_conversation_id = response_doc["conversation_id"] # Use the ID returned by API

    current_history.append({"role": "assistant", "content": answer})

    await save_chat_data(chat_id, current_history, new_conversation_id, user_info_dict)

    try:
        await update.message.reply_text(answer, parse_mode=ParseMode.MARKDOWN)
    except Exception as e:
        logger.warning(f"Failed to send Markdown message to chat {chat_id}: {e}. Retrying with plain text.")
        try:
            await update.message.reply_text(answer)
        except Exception as fallback_e:
            logger.error(f"Failed to send fallback plain text message to chat {chat_id}: {fallback_e}", exc_info=True)


def format_history_for_api(messages: list) -> list:
    """
    Converts internal history format [{'role': 'user', 'content': '...'}, ...]
    to the API required format [{'prompt': '...', 'response': '...'}, ...].
    Ensures only valid pairs are included.
    """
    api_history = []
    i = 0
    while i < len(messages):
        # Look for a user message
        if messages[i].get("role") == "user" and "content" in messages[i]:
            prompt_content = messages[i]["content"]
            response_content = None
            # Check if the next message is a corresponding assistant response
            if i + 1 < len(messages) and messages[i+1].get("role") == "assistant" and "content" in messages[i+1]:
                response_content = messages[i+1]["content"]
                # Add the pair to history
                api_history.append({"prompt": prompt_content, "response": response_content})
                i += 2 # Move past both user and assistant message
            else:
                i += 1 # Move past the user message only
        else:
            i += 1
    return api_history


def main() -> None:
    """Start the bot."""
    if not TOKEN:
        logger.critical("TELEGRAM_BOT_TOKEN environment variable not set! Exiting.")
        return
    if not API_KEY:
        logger.warning("API_KEY environment variable not set! DocsGPT API calls will fail.")
    if STORAGE_TYPE == "mongodb" and mongo_collection is None:
        logger.critical("MongoDB storage configured but connection failed. Exiting.")
        return

    logger.info(f"Initializing Telegram Bot Application with storage type: {STORAGE_TYPE}")
    application = Application.builder().token(TOKEN).build()

    application.add_handler(CommandHandler("start", start))
    application.add_handler(CommandHandler("help", help_command))
    application.add_handler(MessageHandler(filters.TEXT & ~filters.COMMAND & filters.UpdateType.MESSAGE, echo))

    logger.info("Starting Telegram bot polling...")
    application.run_polling(allowed_updates=Update.ALL_TYPES)

    if mongo_client:
        logger.info("Closing MongoDB connection.")
        mongo_client.close()

    logger.info("Telegram bot stopped.")


if __name__ == "__main__":
    main()