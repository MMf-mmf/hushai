# Project Ahithophel

Knowledge is power. There is a vast amount of knowledge available, but people are fallible—even the most intelligent individuals can make mistakes. Errors often occur not from a lack of knowledge, but because the right action slips the mind at a critical moment.

The aim of this project is to provide a "genius advisor" available 24/7 to observe your life, understand your personal context, and advise you on business, personal matters, and more.

## Plan for our first Advanced Agent that utilizes The Ahithophel Framework

1. **Data Collection**
   Gather data about the user's life, preferences, and knowledge. 
   - For the initial launch, the user will start with a prompt describing their situation. If the context is clear and actionable, the system will proceed to the answering phase. If not, Ahithophel will ask follow-up questions to build a complete understanding before generating advice.
   - **Future Plans:** Data collection will eventually expand significantly. We plan to utilize cameras and audio recordings to observe the user's life and build a detailed data profile. This will include third-party integrations connecting to the user's social media profiles. If a query involves other individuals, the system will gather public data on them as well, allowing it to evaluate the situation comprehensively and provide highly contextual advice.

2. **Knowledge Base**
   We must curate specific books and resources for the AI to draw from.
   - We will start with a single foundational book. The AI will determine which chapters are relevant to the user's query and apply those principles. However, because concepts in one chapter often rely on another, the system must avoid "tunnel vision" and ensure it doesn't lock itself too strictly to a single chapter, which could cause it to miss important real-world applications.
   - **Future Plans:** Expand the knowledge base to include behavioral psychology and temperament. For example, once the primary agent determines a plan of action, a secondary "Executor Agent" can refine and tailor that plan specifically to the personalities of the people involved.

## Agent Architecture and Roles

### Data Collection Agents
1. **Min-Info Agent:** Determines if the user has provided the bare minimum information required to operate. 
   - *Example:* If the user says, "I walked into my house," the system recognizes this is insufficient and prompts for more detail. If the user says, "I walked into my house and found my wife in bed with another man," the situation is clear-cut, and the system moves to the next phase.
2. **Yenta Agent:** Once the minimum information threshold is met, this agent determines if further context is needed. It asks targeted follow-up questions to deepen its understanding. 
   - *Example:* In the scenario above, it might ask, "How long have you been married?", "Do you have children?", "What is your current financial situation?", or "What is your emotional state?"
3. **Message Refiner Agent:** Takes the user's finalized input, condenses it, corrects any spelling or grammar mistakes, and passes a clean, refined prompt to the Answering phase.

### Answering Agents
4. **Traffic Controller Agent:** Analyzes the refined question and routes it to the correct books or chapters within the knowledge base.
5. **Answer Agent:** Takes the Traffic Controller's suggested context and drafts an initial answer.
6. **Answer Controller Agent:** Evaluates the draft to verify that it actually answers the user's core question.
7. **Answer Refiner Agent:** Edits the answer to be clear and concise. Before delivering it to the user, it passes the draft back to the Traffic Controller Agent to see if the proposed solution triggers the need for *new* books/chapters. This feedback loop can run up to a maximum of 3 to 5 times.
8. **Temperament Agent:** Once a plan of action is determined, this agent refines the advice to be more tailored to the personalities and temperaments of the individuals involved.

### Memory Agents
9. **Memory Agent:** Takes the final Q&A and stores it so it can be easily accessed in the future. This builds a persistent, long-term profile of the user's life and the advice given, helping the system identify behavioral patterns over time.
10. **Memory Retrieval Agent:** Responsible for surfacing relevant past context when a new question is asked. 
   - *Example:* If the user asks, "What should I do about my marriage?", this agent retrieves past interactions regarding their spouse and feeds them to the Traffic Controller to ensure the new advice aligns with historical context.

### Research Agent
11. **Research Agent:** Allows the system to ask questions of *itself* rather than the user. If the core knowledge base is insufficient, this agent can independently research scientific literature or external data to open up new avenues of thought. This prevents the system from getting stuck in local minimums, ensures advice is backed by up-to-date research, and expands the AI's problem-solving capabilities.


## PROJECT Hushai (The data intake system)

the idea for Hushai, just like in the Bible story, is a double agent. A double agent gathers information with an unparalleled ability to hear and see everything.

the idea here for this app is to help us gather as much information as possible by having a mobile application with its camera and voice recording on and capturing every detail

the key is in every detail. we will need a way to process all this information
we will need to create a Rust server back end. the back end's responsibility is to ingest this massive amount of data and ingest it in a way that can be used to analyze the person and their relationships to better help them

all the data will be stored, video will be analyzed for visual scenario categorization, and objects will be created with data points such as the timestamp, person, action, vibes, disposition, and more if available
the audio will be transcribed, split into sentences with timestamps, and embedded

to get a bit more into the disposition bit there will need to be an AI that can analyze every bit of the data. now there might be an AI model that can transcribe and understand the audio's emotional context as well, such as if the person is angry, down, happy, etc.


> Note: every few hours or number of words will need to be summarized and embedded so that any top future-level queries can perhaps reference it, kind of like a book, so that we don't need to summarize the entire day every time. We have an agent that can benefit from it.



### Post data intake

once all the data is in we can then have multiple agents working in parallel
such as the above first idea where it focuses on the "Yes!" books approach and others to figure out the best way to help the target person with really anything they need help with, business, relationships, and more. Now that the AI has the full context, it can actually help





### Privacy Privacy Privacy

the backend will use all local models running locally on the user's computer, and the app will be sending the audio and video through a local server for now to avoid things getting on the web
we will need the best and fastest local models for speech to text, for embeddings, for video recognition, to recognize people, and more..




# Actionable steps (described in terms of different teams working in parallel)
in order to put this all together we will need a modular approach to development and to the system architecture as a whole,
so that the cameras for example or should we call it data intake can be many different types from dashcam/gopro style cameras or any other camera can be used for the intake and to pass on that data to the backend server in a standardized format the backend doesn't care about the intake method so so perhaps this is a project for itself to create a standardized data format to intake data from different cameras audio and video but that can be a separate task that a team can work on in parallel to the backend server development and the agent development, so we can have multiple teams working on different parts of the system at the same time and then we can integrate it all together once we have the different pieces ready.
# Our general tech stack is A rust backend and a native android mobile application for the camera and audio everything else will be using the rust ecosystem for the backend and the AI models and the agents and everything else.
- first we have the audio and video part of the app that can capture continuous audio and video and stream it to a server as well as store it locally if unable to stream then only to upload the local data when the connection is back.. there will be quite a bit of overlap/cordination needed between the app development team and the backend server team to make sure that the data is being sent in a format that the backend can understand and process efficiently.

- the backend server will need to be able to handle the massive amount of data being sent to it from multiple sources ie multiple cameras.
- when working with multiple sources it would be nice to be able to glue the cameras together so that the AI can better understand the context of the situation, for example if one camera is in the living room and another camera is in the kitchen and they both capture the same event from different angles, it would be nice to be able to combine that data to get a better understanding of the situation.
this task is from one team since there is a lot of work to be done on this end from identifying overlapping data frames such as that done in military drones to create a wider image and put together the images of the same event from different angles...
1. store the data in a structured format that can be easily queried and analyzed.
2. Have queues that process the incoming data, to elaborate on how this should work we will have say 3 services that need to process the incoming data to then write to the main database. 1. audio transcription and sentiment analysis 2. audio embedding 3. video analysis
all of them will take a different amount of time to process.70j



# Tasks to do now:
1.[✅] create a create issue claude skill that create a github issue in a specific format 
the format is as follows:
- **Title**: [Project Name] - [Task Description]
- **Description**: A detailed description of the task, including any relevant information, requirements,
- **Acceptance Criteria**: at what point is this task complete?
- **How to Test**: how can we test to make sure the task is complete and working as expected, part of the format is to test the real thing some how or another but it must be tested not just with unit or integration tests but with the real thing as well, so we can be sure it works in the real world and not just in a test environment.


2. [] come up with a real plan for how the backed is going to ingest the camera data from at least 2 sources at the same time and from different camera sources as well the first will be the android application and the second will be a simple webcam setup the idea is to start with two camera sources so that we make sure that the intake system is modular enough to handle multiple sources and not get to stuck on just one type of camera since its crucial part of the program to overtime, expand the amount of intake cameras we have to enlarge array of different types once we have a clear plan we can start writing up the tickets for the initial mobile app and the initial backend server


3. [] Create the ticket to create the native adroid application and the axum rust backend server that will intake the data and store it in a database.



TODO:
- [] create enable audio only mode where it does not save the video but only the audio since some times the vid is not needed this will save on storage space and processing time....
- [] create a browser based application where we can see the video and audio that we captured so that the admin can quickly browser through and find audio and video from a given time, it should act like any other flagship security camera software where it stitches together the clips so that the admin can view the video as one video and browse through the timestamps not knowing what is under the hood and how the data is stored.
you can test it by obviously setting up everything locally and then opening the browser and getting my input when you're ready so that I can validate the work

- [] add a off button to the app since it seems right now it will just always run we need to work on the UI as well at the app. Things are cluttered and not very neat and it's all black and some of the text is pretty dark as well so it's hard to see.

- [] currently our voice detection is working poorly and i'm not quite sure what the issue is all i know is that yesterday i registerd my voice and now when opening the app id did not register my voice when asking a question and then ignored my voice as if i never registerd it..

- [] currently the voice assistant has a very old un natural voice lets get a better more modern voice it should run locally reach the best one and get it hooked up..

- [] now that we got the ability to control the phone wirelessly, let's try to do it over the USB so that we don't need to be connected on the same network. If that doesn't work we can fall back to the previous method that has been working..

- [] security we need to encrypt the audio and video files so that only the admin can actually view them..
- [] we really need to advance the our webapplication big time so that we can do everything that we already do as far as scrubbing through the video
and add section for a full chat interface where we can have a chat over all our video record, recordings, and audio record recordings. In the future, we would like to then incorporate different agents that the user can select that I are basically like chat Windows, but with a specific focus and capabilities...
- [] it should have all the abilities of the mobile app to record audio video if we so choose, we should be able to analyze the voices everything that we can do on the mobile application so please write up a ticket for this functionality


- [] currently the open voices on the app has many different open voices and many of them are all the same person just with statick in the background
    Which is unsustainable to have every bit of static come up as a unknown speaker, and then have to manually merge it into an existing voice. We need to get a serious improvement on this end when it comes to uniquely identifying voices


- [] its time to start to add perhaps a new Agent that has the ability to answer question like (how has my conversational skills being.) (how productive have I been? What improvements can you give me?) that will have the entire context of my life and be able to answer these private questions given the access of data. I have given it so it will need to have a broad understanding of the entire transcription time dates times stamps and different people in the conversation.


- [] improve the voice and chat assistant so that when i ask it a question it should not return any id's or or other time stamps rather it should be human readable data for example, don't give a timestamp but say yesterday at 5 o'clock or three days ago at 2 aclock for example another would be don't say user and then the user ID would say the user's name if we have one and if we don't just say it's an unidentified user
Note this should be across the mobile and Web App.


- improve the chat in the webapplicatoin so that 
  - 1 it seems to chat window seems to be a bit off center the send button and part of the chat are out of the screen view
  - we want to be able to clear the chat and start a new one
  - we should have the ability to ask regarding just a single camera or across all cameras and all recordings..
  - I don't see a settings menu where I can tag different speakers like we have in the app however, I should mention the app is quite broken. It does not have all the speakers there the last time I checked it seems like it's still holding the same old speakers/voices




- Now we need to get this application ready begint he opens source project bit
- we will need to create the needed github repo (Private)
- there are a lot of files all over the place lots of debt code and things are not documented linearly so that someone else can set this up locally we need to clean up the code base write things up line and read me, etc..
- we need to have a technical read me on what is going on in the background, etc.


- now lets continue building out our application and add deep a AI vision capabilities
 You'll do research and see that our application already has voice detection to link people to the audio and video clips and then exposes it to our AI assistance. Now let's expand it to give it vision capabilities. We should be able to identify people in the frames identify what people are doing in the frames as well as identify objects what the objects are and add all that Meta data to the video audio so that we can then continue building out our smart agent so that our smart agent can then answer questions such as when did I see a car or when did I see a car with a license plate of X or when did I see this in this person?
 We must plan carefully to use only the most up-to-date models for all of our tasks so that we get the best results running locally on our computer in line with the rest of our project





# NEW immediate tasks

 - do a thorough investigation into the application to understand it's dependencies and we need to see if we can bundle this in a docker file so that we could just do docker compose up and everything spins up and runs correctly. We have had in the few in the past times that certain servers were not connected or certain points were not running, and we have to manually start them up, and this should hopefully solve it.

 - Now that we added vision when viewing the video we need the ability to see the objects we detected so that when viewing the video in the browser applications there should be a tab to view the video with object detection and in that mode it should have the classic boxes around the objects with its labels. If it's a person, it should say a person and it's name if it's identified or unidentified, and then all the objects that it identifies should obviously be marked as such.


- We need to have the ability to record offline (not connected to the mother ship) where we have no connection that is sending off the data live so that when the connection is cut/has hiccups/is officially offline to allow the users to upload audio/videos in so for the mobile application if it's not connected, it should continue recording and storing it data locally all while clearly displaying on the screen that it is now storing data locally along with data of how much memory is left on disc and then as soon as the connection does occur, we should try to offload all the local data back to the server and then delete what we uploaded..


Simulate high load to see how the system handles it will it be able to ingest 30 video cameras at once?
 Will the transcription and other AI processes be queued correctly? so that it slowly or hopefully sooner than later does process everything?

 - [] We must now add video and audio capture functionality to the web application as well so that we can turn on video capture just like we are doing in the mobile app.
 Like this we go for the first time being able to actually have two video sources streaming in at the same time for our test.
 In order for this task to be complete, we must have the mobile app and the mobile Web App stream video and audio and make sure that it's being processed correctly.

# Bugs and needed updates
- [] in the Known peaple section it has says:
People
name & merge the faces in your recordings
Refresh
✕
Known people (1)
Mendel
Mendel
20 sightings · last seen 6/28/2026, 4:06:11 PM
which is only partially correct since it was not 20 sightings. It was only seemingly one sighting in one single video...
I'm not sure where I got the number 20. Maybe it's from 20 video segments definitely something that needs to be locked into unfixed since it was in fact, part of the same few second clip

- [] under the known people and known voices, if someone was already identified, it should be in a drop-down so that it only displays the unknowns for us to select since overtime with the amount of known voices and known people grow, it is not important to have all of them in a drop-down as soon as we open up the tab only if we actually want to view them.

- We must lean deeply into the face detection feature and license detection feature we need to make sure that whenever we identify a person or a car, we clean up the image zoom and do the necessary cropping to try to get the most clear image so that we can then process it correctly and categorize it correctly. We should follow the most sophisticated industry standards if necessary so that we get the best results compute is not a factor over here. We need the best results.

 - Test Test Test test the application end to end and report bugs and needed improvements
   we need to come up with a way to automate the testing as much a possible
   so that the phone and computer are controled and clicked 
   - it should take basic vid with the camera and play A known video at the same time on the computer where we know what is in the video and what the audio video supposed to look like so that we can then confirm that things were recorded correctly that the correct objects were detected, etc...
   We'll have a set up where the phone is pointing at the computer and will take a video of the computer screen that pulls up the video at the exact same time that it's needed all of it should be automated and end so that a user does not need to click one button. It should all be automated 100%.

- get images of a nice app so that we can update the mobile apps Ui based on it such as get loads of images from the nest mobile app 
 # Future plan 
 - have a agent scan social or integrate with 3rd party software to add a lable to the unlabeled peaple



# Production ready (the must haves for this to be production ready)
- [] if this is going to run on the local network it must set up some sort of encryption so that not anyone on the network can the application front end backend.
as you can see we can connect cameras over the local wifi network so this needs to be further secured
in addition we are talking about the admin panel should be perhaps locket to go given computers ip and not just any computer on the network...
- [x] instead of connecting to http://127.0.0.1:8070/ in the browser we need to be able to write a real name in the url that sill points to the same port only it looks nicer to a user
  → Done: the viewer is reachable at **https://hushai.local/** (no port). `local_dev/setup_hostname.sh` sets the Mac's Bonjour name to `hushai` + a pf 443→8070 redirect; `run_stack.sh --lan` binds the LAN + allowlists this host; the TLS cert (gen_certs.sh) is already issued for `hushai.local`. See AGENTS.md "LAN security model" → "Friendly admin URL".
- [] it must have a correct storage plan for long term storage for this we should go with the typical industry gold standard
- we must be able to capture video from up to 30 cameras at once (we must do research on computer hardware needed to make this possible such as if i want it to all be sending the video feed with a cable 


# A must for the developers 
- for us to go live with a gentic workflows this meens the developers is an ai agent which meens the ai must be able to fully maintain the application for this to be done we need to break the application down into its parts and have Agents review each part of the application 
   - a update agent that looks over all the dependence/the ai models and others to see if there is a newer model that is more efficient and faster Wyoming maintaining the existing quality or better for the same compute for example
   and before actualy doing any update we must varify everting works end to end...


   


PASSWORD: hushai-dev